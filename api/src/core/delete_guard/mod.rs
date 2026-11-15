//! The one check every deletion of a music file goes through (design section 13).

use crate::auth::UserId;
use crate::core::database_failure;
use crate::folder_policy::PolicyRows;
use crate::import_queue::ImportQueue;
use crate::models::deletion_review::{DeletionRecord, DeletionReviewRow};
use crate::models::discovery_playlist::DiscoveryTrackRow;
use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage};
use shared::navidrome::{DeletionReason, DiscoveryTrack};
use soulbeet::navidrome::models::NativeSong;
use soulbeet::{ForgetOutcome, ImportedItem, MusicImporter};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use tracing::{info, warn};
use unlink::UnlinkError;

mod listeners;
pub mod paths;
mod rules;
pub(crate) mod staging;
#[cfg(test)]
pub(crate) mod test_support;
mod unlink;

pub use listeners::{Listeners, LiveListeners};

/// The guard over Soulful's database, Navidrome as each user sees it, and the local path of
/// Navidrome's default library when it is set (D8). One guard serves one run (a rating sync, or
/// the removals of one import): Navidrome's user list, once rule 4 needs it, is read once per
/// guard.
pub struct Guard<'a> {
    pub db: &'a SqlitePool,
    pub listeners: &'a dyn Listeners,
    pub music_path: Option<&'a Path>,
    /// Navidrome's users as the first linked admin lists them, or why they cannot be listed.
    navidrome_users: tokio::sync::OnceCell<Result<Vec<String>, Failure>>,
}

impl<'a> Guard<'a> {
    pub fn new(
        db: &'a SqlitePool,
        listeners: &'a dyn Listeners,
        music_path: Option<&'a Path>,
    ) -> Guard<'a> {
        Guard {
            db,
            listeners,
            music_path,
            navidrome_users: tokio::sync::OnceCell::new(),
        }
    }
}

impl Guard<'static> {
    /// The guard on Soulful's database, each user's stored credentials and CONFIG's music path.
    pub fn live() -> Guard<'static> {
        Guard::new(
            &crate::db::DB,
            &LiveListeners,
            crate::config::CONFIG.navidrome_music_path(),
        )
    }
}

/// What may be removed for a song.
#[derive(Debug)]
pub enum Verdict {
    /// Every rule allows removing this library file.
    Library(Checked),
    /// The file is a pending discovery track of the caller, whose own rating of the song is
    /// `rating`; discovery's staging rules apply (section 13).
    Discovery { track: DiscoveryTrack, rating: u8 },
}

/// A library file every rule allows removing.
#[derive(Debug)]
pub struct Checked {
    /// The caller's native record of the song, as read after the rating was re-sent.
    pub song: NativeSong,
    /// The canonical file.
    pub path: PathBuf,
    /// The canonical deepest registered folder holding it, the prune root.
    pub folder: PathBuf,
    /// The file's identity when it was checked, compared when it is removed (rule 6).
    id: unlink::FileId,
}

/// What started a removal, as the audit records it (D13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Someone in the web UI.
    Ui,
    /// Soulful on its own, from a rating sync.
    Auto,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Ui => "ui",
            Origin::Auto => "auto",
        }
    }
}

/// How a removed file leaves the library of its folder.
pub enum ForgetVia<'a> {
    /// Queue the forget on the folder's worker in a spawned task that owns this handle, and do
    /// not wait (auto-delete: the audit row never waits on beets, step 3 final ruling; a dropped
    /// request cannot cancel the forget). The task prunes the parents after the forget.
    Queue(ImportQueue),
    /// Forget now, from inside the worker that owns `library`.
    Worker {
        importer: &'a dyn MusicImporter,
        library: &'a Path,
    },
}

impl Guard<'_> {
    /// Resolves `song_id` from `actor`'s native record, hands back a pending discovery track of
    /// `actor` by song id before any path is resolved, then by its stored path canonicalized
    /// equal to the song's file (D12), and checks a library file with check_resolved (D5).
    pub async fn check_song(&self, actor: &UserId, song_id: &str) -> Result<Verdict, Failure> {
        let song = self
            .listeners
            .native_song(actor, song_id)
            .await
            .map_err(|e| e.failure(Stage::Delete, e.to_string()))?
            .ok_or_else(song_not_found)?;
        let pending = DiscoveryTrackRow::pending_for_user_in(self.db, &actor.to_string())
            .await
            .map_err(|e| database_failure(Stage::Delete, e))?;
        if let Some(track) = pending
            .iter()
            .find(|t| t.song_id.as_deref() == Some(song.id.as_str()))
        {
            return Ok(Verdict::Discovery {
                track: track.clone(),
                rating: song.rating,
            });
        }
        let (path, id) = paths::local_file_learning(self.db, &song, self.music_path, actor).await?;
        for track in pending {
            if tokio::fs::canonicalize(&track.path)
                .await
                .is_ok_and(|staged| staged == path)
            {
                return Ok(Verdict::Discovery {
                    track,
                    rating: song.rating,
                });
            }
        }
        self.check_resolved(actor, song, path, id, &[])
            .await
            .map(Verdict::Library)
    }

    /// Checks the library item beets reported at `library.join(&item.path)`: its song is the
    /// first song of `actor`'s search for the item's tags whose path on this machine names the
    /// canonical file (paths::names_file), and that song's own refusal from local_file_in
    /// stands. When no song names it, the songs is_imported_file takes for the item teach where
    /// their libraries are from the item's file, as an import's index lookup does, when their
    /// library has no root on this machine (paths::learn_in_check), and when one is learned the
    /// songs are asked again. When still none
    /// names it, the refusal of a song is_imported_file takes for the item but whose file cannot
    /// be found here says why, else Navidrome has no song for it. Then check_resolved with
    /// `exempt_jobs`. A pending discovery track never holds a library item, so this is Checked.
    pub async fn check_file(
        &self,
        actor: &UserId,
        item: &ImportedItem,
        library: &Path,
        exempt_jobs: &[String],
    ) -> Result<Checked, Failure> {
        let reported = library.join(&item.path);
        let no_file = |e: std::io::Error| {
            path_unresolved(format!("No file at {}", reported.display())).with_cause(
                Service::Filesystem,
                None,
                e.to_string(),
            )
        };
        let path = tokio::fs::canonicalize(&reported).await.map_err(no_file)?;
        let size = tokio::fs::metadata(&path).await.map_err(no_file)?.len();
        let candidates = self
            .listeners
            .songs_for_item(actor, item)
            .await
            .map_err(|e| e.failure(Stage::Delete, e.to_string()))?
            .ok_or_else(|| {
                path_unresolved(format!(
                    "Navidrome cannot be searched for the tags of {}",
                    path.display()
                ))
            })?;
        let taken: Vec<NativeSong> = candidates
            .iter()
            .filter(|song| crate::core::index::is_imported_file(song, &item.path, size))
            .cloned()
            .collect();
        let mut named = self.song_naming(&candidates, &path).await?;
        if named.is_none() && !taken.is_empty() {
            match paths::learn_in_check(self.db, &taken, &reported, self.music_path).await {
                Ok(learned) if !learned.is_empty() => {
                    named = self.song_naming(&candidates, &path).await?;
                }
                Ok(_) => {}
                Err(e) => warn!(
                    "Could not learn where Navidrome's libraries are on this machine from {}: {e}",
                    reported.display()
                ),
            }
        }
        if let Some(song) = named {
            let (file, id) = paths::local_file_in(self.db, song, self.music_path).await?;
            if file != path {
                return Err(changed_since_check());
            }
            return self
                .check_resolved(actor, song.clone(), path, id, exempt_jobs)
                .await;
        }
        // The first song taken for the item that has no file here says why.
        for song in &taken {
            paths::local_file_in(self.db, song, self.music_path).await?;
        }
        Err(path_unresolved(format!(
            "Navidrome has no song for {}",
            path.display()
        )))
    }

    /// The first of `candidates` whose path on this machine names the canonical file `path`
    /// (paths::names_file).
    async fn song_naming<'s>(
        &self,
        candidates: &'s [NativeSong],
        path: &Path,
    ) -> Result<Option<&'s NativeSong>, Failure> {
        for song in candidates {
            if paths::names_file(self.db, song, self.music_path, path).await? {
                return Ok(Some(song));
            }
        }
        Ok(None)
    }

    /// Rules 1, 2, 5, the one-song check, 3 and 4, in that order, for `path`, the canonical
    /// file local_file_in gave for `song`, whose identity it read as `id`. Jobs in
    /// `exempt_jobs` do not count for rule 5.
    async fn check_resolved(
        &self,
        actor: &UserId,
        song: NativeSong,
        path: PathBuf,
        id: unlink::FileId,
        exempt_jobs: &[String],
    ) -> Result<Checked, Failure> {
        let rows = PolicyRows::load_in(self.db)
            .await
            .map_err(|e| database_failure(Stage::Delete, e))?;
        let (folders, _) = rows.inputs();
        let folder = rules::owning_folders(&path, &folders, actor)?.to_path_buf();
        rules::audio_file(&path)?;
        rules::not_in_use_in(
            self.db,
            &rules::holding_folders(&path, &folders),
            &song,
            exempt_jobs,
        )
        .await?;
        self.one_song(actor, &song, &path).await?;
        let others = self.others_permit(actor, &song, &path).await?;
        let song = self.no_unknown_rater(actor, &song, &others).await?;
        Ok(Checked {
            song,
            path,
            folder,
            id,
        })
    }

    /// Rule 6 and the removal: the audit row (actor, song, title, artist, album, path, the
    /// caller's rating, origin, reason) in a transaction; unlink_if_same(checked.path, checked.id);
    /// commit; one info line "Deleted {path} ({reason}, {origin})"; then the forget and pruning
    /// the empty parents up to checked.folder. Ok holds the deletion's warnings: a commit that
    /// failed (logged where it happens), and with `ForgetVia::Worker` a forget that did not
    /// remove one item (with `ForgetVia::Queue` that one is logged). Nothing may use `self.db`
    /// while the transaction is open, so the row is built before it begins. The unlink, the
    /// commit, its warning, the info line and the spawn of a queued forget run in one spawned
    /// task, which a dropped caller does not cancel. A removal that stops without saying whether
    /// it unlinked is settled by lost_removal: when nothing is at the path the removal is done
    /// and goes on to its forget and pruning; when another file is there it is done without
    /// them (Left::AnotherFile); when the file is still there it is Kept; when it cannot be
    /// looked at it is Unconfirmed.
    pub async fn carry_out(
        &self,
        actor: &UserId,
        checked: Checked,
        reason: DeletionReason,
        origin: Origin,
        forget: ForgetVia<'_>,
    ) -> Result<Vec<String>, NotRemoved> {
        let Checked {
            song,
            path,
            folder,
            id,
        } = checked;
        let record = record_of(actor, song, &path, reason, origin);
        let mut tx = self
            .db
            .begin()
            .await
            .map_err(|e| NotRemoved::Kept(database_failure(Stage::Delete, e)))?;
        DeletionReviewRow::insert_in(&mut tx, &record)
            .await
            .map_err(|e| NotRemoved::Kept(database_failure(Stage::Delete, e)))?;
        let (queued, in_worker) = match forget {
            ForgetVia::Queue(queue) => (Some(queue), None),
            ForgetVia::Worker { importer, library } => (None, Some((importer, library))),
        };
        // For a task that stops: the caller then queues the forget itself.
        let requeue = queued.clone();
        let (db, target, root, kept) = (
            self.db.clone(),
            path.clone(),
            folder.clone(),
            record.clone(),
        );
        // One task unlinks, commits and, for a queued forget, spawns it, so a dropped caller can
        // neither delete the file and roll its row back nor leave the file in beets' library.
        // A refusal drops the transaction, which rolls the row back.
        let removal = tokio::spawn(async move {
            let unlinking = target.clone();
            let (unrecorded, left) =
                match tokio::task::spawn_blocking(move || unlink::unlink_if_same(&unlinking, id))
                    .await
                {
                    Ok(Ok(())) => (
                        tx.commit().await.err().map(|e| {
                            let warning = not_recorded(&target, &e.to_string());
                            warn!("{warning}");
                            warning
                        }),
                        Left::Nothing,
                    ),
                    Ok(Err(UnlinkError::Changed)) => {
                        return Err(NotRemoved::Kept(changed_since_check()))
                    }
                    Ok(Err(UnlinkError::Io(e))) => {
                        return Err(NotRemoved::Kept(delete_failed(&target, &e)))
                    }
                    Err(e) => {
                        drop(tx);
                        lost_removal(&db, &kept, &target, id, &e.to_string()).await?
                    }
                };
            info!(
                "Deleted {} ({}, {})",
                target.display(),
                reason.as_str(),
                origin.as_str()
            );
            if let Some(queue) = queued {
                tokio::spawn(forget_queued(queue, target, root, left));
            }
            Ok((unrecorded, left))
        });
        let mut warnings = Vec::new();
        let left = match removal.await {
            Ok(Ok((unrecorded, left))) => {
                warnings.extend(unrecorded);
                left
            }
            Ok(Err(f)) => return Err(f),
            Err(e) => {
                let (unrecorded, left) =
                    lost_removal(self.db, &record, &path, id, &e.to_string()).await?;
                warnings.extend(unrecorded);
                if let Some(queue) = requeue {
                    tokio::spawn(forget_queued(queue, path.clone(), folder.clone(), left));
                }
                left
            }
        };
        if let Some((importer, library)) = in_worker {
            warnings.extend(forget_in_worker(importer, library, &path, &folder, left).await);
        }
        Ok(warnings)
    }

    /// Navidrome lists the canonical file `path` as `song` only, in `actor`'s view: their
    /// search for its tags finds `song`, and no other song it finds is or may be the same file
    /// (another_record).
    async fn one_song(
        &self,
        actor: &UserId,
        song: &NativeSong,
        path: &Path,
    ) -> Result<(), Failure> {
        let unsearchable = || {
            path_unresolved(format!(
                "Navidrome cannot be searched for the tags of {}",
                path.display()
            ))
        };
        let candidates = self
            .listeners
            .songs_for_item(actor, &listeners::item_of(song))
            .await
            .map_err(|e| e.failure(Stage::Delete, e.to_string()))?
            .ok_or_else(unsearchable)?;
        if !candidates.iter().any(|c| c.id == song.id) {
            return Err(unsearchable());
        }
        match self.another_record(song, path, &candidates).await? {
            paths::Other::Different => Ok(()),
            paths::Other::Same => Err(path_unresolved(format!(
                "Navidrome lists {} as more than one song",
                path.display()
            ))),
            paths::Other::Unknown => Err(path_unresolved(format!(
                "Navidrome may list {} as more than one song",
                path.display()
            ))),
        }
    }

    /// What the songs of `candidates` other than `song` are to its canonical file `path`
    /// (paths::other_in): the first that is or may be that file, else Different.
    async fn another_record(
        &self,
        song: &NativeSong,
        path: &Path,
        candidates: &[NativeSong],
    ) -> Result<paths::Other, Failure> {
        for candidate in candidates.iter().filter(|c| c.id != song.id) {
            let other = paths::other_in(self.db, self.music_path, song, path, candidate).await?;
            if other != paths::Other::Different {
                return Ok(other);
            }
        }
        Ok(paths::Other::Different)
    }
}

/// The audit row of `actor`'s removal of `song`'s file `path`, with a new id.
fn record_of(
    actor: &UserId,
    song: NativeSong,
    path: &Path,
    reason: DeletionReason,
    origin: Origin,
) -> DeletionRecord {
    DeletionRecord {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: actor.to_string(),
        rating: (song.rating > 0).then_some(song.rating),
        song_id: song.id,
        title: song.title,
        artist: song.artist,
        album: song.album,
        path: path.to_string_lossy().into_owned(),
        origin: origin.as_str(),
        reason,
    }
}

/// The warning for a deletion whose audit row could not be stored.
fn not_recorded(path: &Path, error: &str) -> String {
    format!(
        "The deletion of {} was not recorded: {error}",
        path.display()
    )
}

/// Why carry_out did not remove a file.
#[derive(Debug)]
pub enum NotRemoved {
    /// The file stays where it was checked: refused, failed, or a stopped removal that left it.
    Kept(Failure),
    /// A removal that stopped before it reported, and its file could not be looked at: it may be
    /// gone.
    Unconfirmed(Failure),
}

impl NotRemoved {
    pub fn into_failure(self) -> Failure {
        match self {
            NotRemoved::Kept(f) | NotRemoved::Unconfirmed(f) => f,
        }
    }
}

/// What a removal left at the path of its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Left {
    /// Nothing: the file's beets item is forgotten and the folders left empty are pruned.
    Nothing,
    /// Another file, which a removal that stopped finds there: beets' item at the path may be
    /// that file's, so it is not forgotten, and nothing is pruned.
    AnotherFile,
}

/// Settles a removal of the checked file `path` (identity `id`) that stopped (`error`) without
/// saying whether its unlink ran, and makes the audit match the file (unlink::look): when the
/// file is still there nothing was removed, no row stays, and it is Kept with delete_failed;
/// when it is gone, with nothing or another file at its path, the removal is done and its row
/// is recorded once (`record` keeps its id), and Ok holds the warning when that row could not
/// be stored (logged here) and what is at the path; when the path cannot be read nothing is
/// recorded and it is Unconfirmed: the file may have been deleted.
async fn lost_removal(
    db: &SqlitePool,
    record: &DeletionRecord,
    path: &Path,
    id: unlink::FileId,
    error: &str,
) -> Result<(Option<String>, Left), NotRemoved> {
    warn!(
        "The removal of {} stopped before it reported: {error}",
        path.display()
    );
    let left = match unlink::look(path, id) {
        unlink::Look::There => {
            return Err(NotRemoved::Kept(delete_failed(
                path,
                &std::io::Error::other(error),
            )))
        }
        unlink::Look::Gone => Left::Nothing,
        unlink::Look::Replaced => {
            warn!(
                "Another file is at {} now, so its item stays in beets' library",
                path.display()
            );
            Left::AnotherFile
        }
        unlink::Look::Unreadable(e) => {
            warn!("Could not look at {} after that: {e}", path.display());
            return Err(NotRemoved::Unconfirmed(removal_unconfirmed(path, error)));
        }
    };
    let recorded = match db.acquire().await {
        Ok(mut conn) => DeletionReviewRow::insert_in(&mut conn, record).await,
        Err(e) => Err(e.to_string()),
    };
    let unrecorded = recorded.err().map(|e| {
        let warning = not_recorded(path, &e);
        warn!("{warning}");
        warning
    });
    Ok((unrecorded, left))
}

/// When the removal of `path` left nothing there (`left`): queues its forget on the worker of
/// `folder`'s library, logs a forget that did not remove one item, and prunes the empty parents
/// up to `folder`.
async fn forget_queued(queue: ImportQueue, path: PathBuf, folder: PathBuf, left: Left) {
    if left == Left::AnotherFile {
        return;
    }
    let outcome = queue.forget(&folder, &path).await;
    if let Some(note) = forget_note(&outcome, &path, &folder) {
        warn!("{note}");
    }
    prune(&path, &folder).await;
}

/// When the removal of `path` left nothing there (`left`): forgets it through `importer` in
/// `library`, from inside its worker, and prunes the empty parents up to `folder`; the warning
/// of a forget that did not remove one item.
async fn forget_in_worker(
    importer: &dyn MusicImporter,
    library: &Path,
    path: &Path,
    folder: &Path,
    left: Left,
) -> Option<String> {
    if left == Left::AnotherFile {
        return None;
    }
    let outcome = importer.forget(library, path).await;
    let warning = forget_note(&outcome, path, library);
    prune(path, folder).await;
    warning
}

/// Removes the directories the removal of `path` left empty, up to (not including) `folder`.
/// A directory that cannot be removed is left, as an empty one is harmless, and logged.
async fn prune(path: &Path, folder: &Path) {
    if let Err(e) = pruned(path, folder).await {
        warn!("{e}");
    }
}

/// prune, with why a directory could not be removed.
async fn pruned(path: &Path, folder: &Path) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    crate::server_fns::cleanup_empty_ancestors(parent, folder)
        .await
        .map_err(|e| {
            format!(
                "Could not remove the empty folders above {} up to {}: {e}",
                path.display(),
                folder.display()
            )
        })
}

/// The warning for a forget that did not remove exactly one item, None for Removed.
pub(crate) fn forget_note(outcome: &ForgetOutcome, path: &Path, library: &Path) -> Option<String> {
    let (path, library) = (path.display(), library.display());
    match outcome {
        ForgetOutcome::Removed => None,
        ForgetOutcome::NotInLibrary => Some(format!("{path} was not in the library of {library}")),
        ForgetOutcome::Several(n) => Some(format!(
            "Left {path} in the library of {library}: {n} items have that path"
        )),
        ForgetOutcome::Failed(e) => Some(format!(
            "Could not remove {path} from the library of {library}: {e}"
        )),
    }
}

/// The song's file on this machine cannot be told: `message` says why.
pub(crate) fn path_unresolved(message: impl Into<String>) -> Failure {
    Failure::new(
        FailureCode::DeletePathUnresolved,
        Stage::Delete,
        RetryAdvice::NotRetryable,
        message,
    )
}

/// The caller's Navidrome has no song by that id.
pub(crate) fn song_not_found() -> Failure {
    Failure::new(
        FailureCode::UnknownHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Song not found",
    )
}

/// The file is not in a library folder of the caller: `message` says why.
pub(crate) fn outside_library(message: impl Into<String>) -> Failure {
    Failure::new(
        FailureCode::DeleteOutsideLibrary,
        Stage::Delete,
        RetryAdvice::NotRetryable,
        message,
    )
}

/// The file is not a track: `message` says why.
pub(crate) fn not_a_track(message: impl Into<String>) -> Failure {
    Failure::new(
        FailureCode::DeleteNotATrack,
        Stage::Delete,
        RetryAdvice::NotRetryable,
        message,
    )
}

/// Another listener's mark keeps the file; it never says who or what (section 13 rule 3).
pub(crate) fn blocked_by_other_user() -> Failure {
    Failure::new(
        FailureCode::DeleteBlockedByOtherUser,
        Stage::Delete,
        RetryAdvice::NotRetryable,
        "Another listener rated or starred this track",
    )
}

/// Another listener's marks cannot be read, so the file is kept.
pub(crate) fn veto_unknown() -> Failure {
    Failure::new(
        FailureCode::DeleteVetoUnknown,
        Stage::Delete,
        RetryAdvice::SameSourceLater,
        "Soulful could not read another listener's ratings for this track",
    )
}

/// An unfinished job wants the file (rule 5).
pub(crate) fn in_use() -> Failure {
    Failure::new(
        FailureCode::DeleteInUse,
        Stage::Delete,
        RetryAdvice::SameSourceLater,
        "A download or import that is not finished wants this track",
    )
}

/// The file is not the one that was checked any more (rule 6).
pub(crate) fn changed_since_check() -> Failure {
    Failure::new(
        FailureCode::DeleteChangedSincePreview,
        Stage::Delete,
        RetryAdvice::SameSourceLater,
        "The file changed after it was checked",
    )
}

/// A removal of `path` stopped (`error`) and the file is not where it was checked.
fn removal_unconfirmed(path: &Path, error: &str) -> Failure {
    Failure::new(
        FailureCode::DeleteFailed,
        Stage::Delete,
        RetryAdvice::SameSourceLater,
        format!(
            "{} may have been deleted: the removal stopped before it reported ({error})",
            path.display()
        ),
    )
}

/// The OS refused to remove `path`.
pub(crate) fn delete_failed(path: &Path, e: &std::io::Error) -> Failure {
    Failure::new(
        FailureCode::DeleteFailed,
        Stage::Delete,
        RetryAdvice::SameSourceLater,
        format!("Could not delete {}: {e}", path.display()),
    )
    .with_cause(Service::Filesystem, None, e.to_string())
}

#[cfg(test)]
mod tests {
    use super::test_support::{wanted, FakeNavidrome, Library};
    use super::*;
    use crate::import_queue::test_support::{RecordingDeps, StubImporter};
    use crate::models::navidrome_read_failure::NavidromeReadFailureRow;
    use crate::services::NavidromeLookupError;
    use shared::download::DownloadQuery;
    use shared::failure::{Cause, UserAction, UserActionCode};
    use shared::metadata::Album;
    use soulbeet::NavidromeError;
    use std::sync::Arc;
    use std::time::Duration;

    fn library_verdict(v: Result<Verdict, Failure>) -> Checked {
        match v {
            Ok(Verdict::Library(c)) => c,
            Ok(Verdict::Discovery { track, .. }) => panic!("discovery {track:?}"),
            Err(f) => panic!("{f:?}"),
        }
    }

    async fn checked(lib: &Library) -> Checked {
        library_verdict(lib.guard().check_song(&lib.alice(), "s1").await)
    }

    fn refusal(v: Result<Verdict, Failure>) -> (FailureCode, String) {
        match v {
            Err(f) => (f.code, f.message),
            Ok(_) => panic!("allowed"),
        }
    }

    fn canonical(lib: &Library, rel: &str) -> PathBuf {
        std::fs::canonicalize(lib.root.join(rel)).unwrap()
    }

    const VETO: &str = "Soulful could not read another listener's ratings for this track";

    #[tokio::test]
    async fn a_file_only_its_owner_rated_one_star_is_allowed_after_the_average_is_refreshed() {
        let lib = Library::new().await;
        let checked = library_verdict(lib.guard().check_song(&lib.alice(), "s1").await);
        assert_eq!(checked.path, canonical(&lib, "music/alice/A/x.flac"));
        assert_eq!(checked.folder, canonical(&lib, "music/alice"));
        assert_eq!(
            checked.id,
            unlink::FileId::of(&std::fs::symlink_metadata(&checked.path).unwrap())
        );
        assert_eq!(
            (checked.song.rating, checked.song.average_rating),
            (1, Some(1.0))
        );
        assert_eq!(
            lib.fake.ratings_set(),
            vec![("alice".to_string(), "s1".to_string(), 1)]
        );
        // Ratings of 1 and an average of 1.00 hide no rating above 1: no user list is read.
        assert_eq!(lib.fake.user_lists(), 0);
    }

    const UNKNOWN: &str =
        "Another Navidrome listener Soulful cannot read may have rated this track";

    #[tokio::test]
    async fn an_average_a_rater_soulful_cannot_read_may_hide_in_needs_every_navidrome_user_read() {
        // Alice rated 4, and Carol, a Navidrome user with no Soulful account, rated 4 too: the
        // average is 4.00, the mean of what Soulful reads.
        let lib = Library::new().await;
        lib.fake.rates("alice", "s1", 4);
        lib.fake.hidden_rater("s1", 4);
        let f = lib
            .guard()
            .check_song(&lib.alice(), "s1")
            .await
            .unwrap_err();
        assert_eq!(
            (f.code, f.retry, f.message.as_str()),
            (
                FailureCode::DeleteVetoUnknown,
                RetryAdvice::NeedsUser,
                UNKNOWN
            )
        );
        assert_eq!(
            f.user_action,
            Some(UserAction::new(
                UserActionCode::RelinkNavidrome,
                "Sign in to Soulful once as a Navidrome admin, so Soulful can list Navidrome's users",
                "Soulful sign-in",
            ))
        );
        // Asked of alice and bob, neither an admin.
        assert_eq!(lib.fake.user_lists(), 2);

        // Bob is an admin, and Navidrome lists Carol, whom Soulful has no account for.
        lib.fake.admin("bob");
        lib.fake.users_are(&["alice", "bob", "carol-nd"]);
        let f = lib
            .guard()
            .check_song(&lib.alice(), "s1")
            .await
            .unwrap_err();
        assert_eq!(
            (f.code, f.message.as_str(), f.user_action.map(|a| a.what)),
            (
                FailureCode::DeleteVetoUnknown,
                UNKNOWN,
                Some(
                    "Have every Navidrome user sign in to Soulful once, so Soulful can read their ratings"
                        .to_string()
                )
            )
        );
        assert!(lib.root.join("music/alice/A/x.flac").exists());
    }

    #[tokio::test]
    async fn an_equal_average_passes_when_every_navidrome_user_is_a_linked_account_read_in_the_check(
    ) {
        let lib = Library::new().await;
        lib.fake.rates("alice", "s1", 4);
        lib.fake.admin("bob");
        // Navidrome compares names without regard to case, as Soulful does.
        lib.fake.users_are(&["Alice", "bob"]);
        let guard = lib.guard();
        let checked = library_verdict(guard.check_song(&lib.alice(), "s1").await);
        assert_eq!(checked.song.average_rating, Some(4.0));
        // One guard lists Navidrome's users once: asked of alice, then listed by bob.
        library_verdict(guard.check_song(&lib.alice(), "s1").await);
        assert_eq!(lib.fake.user_lists(), 2);
    }

    #[tokio::test]
    async fn a_linked_listener_who_cannot_see_the_track_keeps_it_without_being_asked_to_sign_in() {
        // Bob signed in to Soulful, but his Navidrome account cannot see the song (per-user
        // libraries): a rating he gave it before stays in the average unread.
        let cannot_see = (
            FailureCode::DeleteVetoUnknown,
            RetryAdvice::NeedsUser,
            "A linked listener's Navidrome account cannot see this track, so Soulful cannot rule out a rating they gave it earlier, and the copy is kept"
                .to_string(),
            Some(UserAction::new(
                UserActionCode::ConfigureNavidrome,
                "Give every Navidrome account linked to Soulful access to this track's library, so Soulful can read their ratings of it",
                "Navidrome's user settings",
            )),
        );
        let lib = Library::new().await;
        lib.fake.rates("alice", "s1", 4);
        lib.fake.admin("bob");
        lib.fake.unsee("bob", "s1");
        let f = lib
            .guard()
            .check_song(&lib.alice(), "s1")
            .await
            .unwrap_err();
        assert_eq!((f.code, f.retry, f.message, f.user_action), cannot_see);

        // With a Navidrome user who never signed in as well, signing in cannot make the check
        // pass, so the account that cannot see the track is what the refusal names.
        lib.fake.users_are(&["alice", "bob", "carol-nd"]);
        let f = lib
            .guard()
            .check_song(&lib.alice(), "s1")
            .await
            .unwrap_err();
        assert_eq!((f.code, f.retry, f.message, f.user_action), cannot_see);
        assert!(lib.root.join("music/alice/A/x.flac").exists());
    }

    #[tokio::test]
    async fn a_user_list_that_cannot_be_read_keeps_the_file() {
        let lib = Library::new().await;
        lib.fake.rates("alice", "s1", 4);
        lib.fake.admin("bob");
        lib.fake
            .users_unlistable(|| NavidromeLookupError::Client(NavidromeError::CircuitOpen));
        let f = lib
            .guard()
            .check_song(&lib.alice(), "s1")
            .await
            .unwrap_err();
        assert_eq!(
            (f.code, f.retry, f.message.as_str(), f.user_action),
            (
                FailureCode::DeleteVetoUnknown,
                RetryAdvice::SameSourceLater,
                UNKNOWN,
                None
            )
        );
        assert_eq!(
            f.cause.map(|c| c.raw),
            Some(NavidromeError::CircuitOpen.to_string())
        );
    }

    #[tokio::test]
    async fn a_stale_average_is_refreshed_before_it_is_compared() {
        // A deleted user's rating stays in the average until someone calls setRating.
        let lib = Library::new().await;
        lib.fake.stale_average("s1", 3.0);
        library_verdict(lib.guard().check_song(&lib.alice(), "s1").await);
    }

    #[tokio::test]
    async fn an_average_with_a_rater_soulful_cannot_read_blocks() {
        let lib = Library::new().await;
        lib.fake.hidden_rater("s1", 4);
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await),
            (
                FailureCode::DeleteBlockedByOtherUser,
                "Another listener rated or starred this track".into()
            )
        );
    }

    #[tokio::test]
    async fn another_users_rating_or_star_of_the_track_or_its_album_blocks() {
        type Mark = fn(&FakeNavidrome);
        let marks: [(Mark, bool); 6] = [
            (|f| f.rates("bob", "s1", 2), true),
            (|f| f.rates("bob", "s1", 1), false),
            (|f| f.stars("bob", "s1"), true),
            (|f| f.rates_album("bob", "al1", 3), true),
            (|f| f.rates_album("bob", "al1", 1), false),
            (|f| f.stars_album("bob", "al1"), true),
        ];
        for (i, (mark, blocks)) in marks.into_iter().enumerate() {
            let lib = Library::new().await;
            mark(&lib.fake);
            let verdict = lib.guard().check_song(&lib.alice(), "s1").await;
            match blocks {
                true => assert_eq!(
                    refusal(verdict).0,
                    FailureCode::DeleteBlockedByOtherUser,
                    "case {i}"
                ),
                false => {
                    library_verdict(verdict);
                }
            }
        }
    }

    #[tokio::test]
    async fn a_song_another_user_cannot_see_does_not_block() {
        let lib = Library::new().await;
        lib.fake.rates("bob", "s1", 5);
        // getSong answers code 70 for bob. bob's rating still counts in Navidrome's average, so
        // rule 4 blocks: a user who rated and lost access is a rater Soulful cannot read.
        lib.fake.unsee("bob", "s1");
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await).0,
            FailureCode::DeleteBlockedByOtherUser
        );
        let lib = Library::new().await;
        lib.fake.unsee("bob", "s1");
        library_verdict(lib.guard().check_song(&lib.alice(), "s1").await);
    }

    #[tokio::test]
    async fn an_unreadable_linked_user_blocks_and_is_named_for_admins() {
        let lib = Library::new().await;
        lib.fake
            .unreadable("bob", || NavidromeLookupError::PasswordUnreadable);
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await),
            (FailureCode::DeleteVetoUnknown, VETO.into())
        );
        let listed = NavidromeReadFailureRow::all_in(&lib.db).await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|l| (l.username.as_str(), l.error.as_str()))
                .collect::<Vec<_>>(),
            vec![("bob", crate::services::STORED_PASSWORD_UNREADABLE)]
        );
        assert_eq!(lib.fake.ratings_set(), vec![]);
        lib.fake.readable("bob");
        library_verdict(lib.guard().check_song(&lib.alice(), "s1").await);
        assert!(NavidromeReadFailureRow::all_in(&lib.db)
            .await
            .unwrap()
            .is_empty());

        let lib = Library::new().await;
        lib.fake.unreadable("bob", || {
            NavidromeLookupError::Client(NavidromeError::CircuitOpen)
        });
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await).0,
            FailureCode::DeleteVetoUnknown
        );
    }

    #[tokio::test]
    async fn every_listener_is_read_so_the_admin_list_is_current_whoever_blocks_first() {
        let lib = Library::new().await;
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, navidrome_token) \
             VALUES ('dave', 'dave', 'x', 'cipher')",
        )
        .execute(&lib.db)
        .await
        .unwrap();
        lib.fake.stars("bob", "s1");
        lib.fake
            .unreadable("dave", || NavidromeLookupError::PasswordUnreadable);
        let unreadable = |lib: &Library| {
            let db = lib.db.clone();
            async move {
                NavidromeReadFailureRow::all_in(&db)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|l| l.username)
                    .collect::<Vec<_>>()
            }
        };
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await).0,
            FailureCode::DeleteBlockedByOtherUser
        );
        assert_eq!(unreadable(&lib).await, vec!["dave".to_string()]);
        lib.fake.readable("dave");
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await).0,
            FailureCode::DeleteBlockedByOtherUser
        );
        assert!(unreadable(&lib).await.is_empty());
        assert_eq!(lib.fake.ratings_set(), vec![]);
    }

    #[tokio::test]
    async fn an_answer_without_the_rating_fields_is_unreadable() {
        for client in [
            FakeNavidrome::minimal_client as fn(&FakeNavidrome, &str),
            FakeNavidrome::legacy_client,
        ] {
            let lib = Library::new().await;
            client(&lib.fake, "bob");
            assert_eq!(
                refusal(lib.guard().check_song(&lib.alice(), "s1").await).0,
                FailureCode::DeleteVetoUnknown
            );
            let listed = NavidromeReadFailureRow::all_in(&lib.db).await.unwrap();
            assert_eq!(
                listed
                    .iter()
                    .map(|l| l.username.as_str())
                    .collect::<Vec<_>>(),
                vec!["bob"]
            );
        }
    }

    #[tokio::test]
    async fn a_visible_song_without_its_album_is_unreadable() {
        let lib = Library::new().await;
        lib.fake.lose_album("al1");
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await),
            (FailureCode::DeleteVetoUnknown, VETO.into())
        );
        let listed = NavidromeReadFailureRow::all_in(&lib.db).await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|l| (l.username.as_str(), l.error.as_str()))
                .collect::<Vec<_>>(),
            vec![(
                "bob",
                "Navidrome does not show this account the album of a song it shows"
            )]
        );
    }

    #[tokio::test]
    async fn a_user_never_linked_to_navidrome_is_not_read() {
        let lib = Library::new().await;
        library_verdict(lib.guard().check_song(&lib.alice(), "s1").await);
        assert_eq!(
            (lib.fake.reads_by("bob") > 0, lib.fake.reads_by("carol")),
            (true, 0)
        );
    }

    #[tokio::test]
    async fn a_symlink_out_of_the_folder_is_refused_before_the_rules() {
        let lib = Library::new().await;
        std::fs::create_dir_all(lib.root.join("outside")).unwrap();
        std::fs::write(lib.root.join("outside/z.flac"), b"audio").unwrap();
        std::os::unix::fs::symlink(
            lib.root.join("outside/z.flac"),
            lib.root.join("music/alice/link.flac"),
        )
        .unwrap();
        lib.fake.file(
            NativeSong {
                id: "s2".into(),
                path: "alice/link.flac".into(),
                size: 5,
                ..lib.record("s1")
            },
            "al1",
        );
        lib.fake.sees("alice", "s2");
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s2").await),
            (
                FailureCode::DeletePathUnresolved,
                "Navidrome's path for this song passes through a symbolic link: alice/link.flac"
                    .into()
            )
        );
        assert!(lib.root.join("outside/z.flac").exists());
    }

    #[tokio::test]
    async fn a_file_outside_every_folder_is_outside_the_library() {
        let lib = Library::new().await;
        lib.song("s2", "carol/z.flac", b"audio");
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s2").await),
            (
                FailureCode::DeleteOutsideLibrary,
                format!(
                    "{} is not inside one of your library folders",
                    canonical(&lib, "music/carol/z.flac").display()
                )
            )
        );
        assert_eq!(lib.fake.ratings_set(), vec![]);
    }

    #[tokio::test]
    async fn a_file_in_another_users_nested_folder_is_theirs_and_a_tie_is_each_ones() {
        // alice's folder `music` is shared over bob's `music/bob`.
        let lib = Library::new().await;
        lib.folder("alice", "alice-top", "music", true).await;
        lib.song("s3", "bob/B/y.flac", b"audio");
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s3").await).0,
            FailureCode::DeleteOutsideLibrary
        );
        // bob's second folder at alice's path: the deepest folder ties, so each owns the file.
        lib.folder("bob", "bob-alice", "music/alice", false).await;
        library_verdict(lib.guard().check_song(&lib.alice(), "s1").await);
    }

    #[tokio::test]
    async fn a_directory_or_a_file_that_is_not_audio_is_not_a_track() {
        let lib = Library::new().await;
        lib.song("s4", "alice/A/cover.jpg", b"jpeg!");
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s4").await),
            (
                FailureCode::DeleteNotATrack,
                format!(
                    "{} is not an audio file",
                    canonical(&lib, "music/alice/A/cover.jpg").display()
                )
            )
        );
        // A directory whose size Navidrome's record happens to give.
        std::fs::create_dir_all(lib.root.join("music/alice/B/dir.flac")).unwrap();
        let size = std::fs::metadata(lib.root.join("music/alice/B/dir.flac"))
            .unwrap()
            .len();
        lib.fake.file(
            NativeSong {
                id: "s5".into(),
                path: "alice/B/dir.flac".into(),
                size,
                ..lib.record("s1")
            },
            "al1",
        );
        lib.fake.sees("alice", "s5");
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s5").await),
            (
                FailureCode::DeleteNotATrack,
                format!(
                    "{} is not a regular file",
                    canonical(&lib, "music/alice/B/dir.flac").display()
                )
            )
        );
    }

    #[tokio::test]
    async fn a_job_that_still_wants_the_recording_keeps_the_file() {
        let lib = Library::new().await;
        // Step 5's jobs row: same folder, same recording by artist and title as normalize_for_matching
        // reads them, still downloading.
        lib.job(
            "j1",
            "alice-music",
            "downloading",
            wanted("marrow  choir", "O Holy Night!", None),
        )
        .await;
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await),
            (
                FailureCode::DeleteInUse,
                "A download or import that is not finished wants this track".into()
            )
        );
        lib.set_job_state("j1", "done").await;
        library_verdict(lib.guard().check_song(&lib.alice(), "s1").await);
    }

    #[tokio::test]
    async fn a_job_the_check_is_made_for_does_not_keep_the_file() {
        let lib = Library::new().await;
        lib.job(
            "j1",
            "alice-music",
            "importing",
            wanted("Marrow Choir", "O Holy Night", None),
        )
        .await;
        let song = lib
            .fake
            .native_song(&lib.alice(), "s1")
            .await
            .unwrap()
            .unwrap();
        let (path, id) = paths::local_file_in(&lib.db, &song, None).await.unwrap();
        let guard = lib.guard();
        let kept = guard
            .check_resolved(&lib.alice(), song.clone(), path.clone(), id, &["j2".into()])
            .await
            .unwrap_err();
        assert_eq!(kept.code, FailureCode::DeleteInUse);
        let checked = guard
            .check_resolved(&lib.alice(), song, path, id, &["j1".into()])
            .await
            .unwrap();
        assert_eq!(checked.path, canonical(&lib, "music/alice/A/x.flac"));
    }

    #[tokio::test]
    async fn rule_five_reads_the_recording_an_album_query_and_fails_closed_on_what_it_cannot_read()
    {
        let mbid = "0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c";
        let album = Album {
            id: "rel-1".into(),
            title: "Winter Songs".into(),
            artist: "Marrow Choir".into(),
            release_date: None,
            mbid: None,
            cover_url: None,
        };
        let cases: [(&str, &str, DownloadQuery, bool); 8] = [
            (
                "alice-music",
                "queued",
                wanted("Someone Else", "Another Title", Some(mbid)),
                true,
            ),
            (
                "alice-music",
                "searching",
                DownloadQuery::new(vec![]).album(album.clone()),
                true,
            ),
            (
                "alice-music",
                "queued",
                DownloadQuery::new(vec![]).album(Album {
                    title: "Summer Songs".into(),
                    ..album.clone()
                }),
                false,
            ),
            // An album job wants every track of its album, hydrated or not.
            (
                "alice-music",
                "downloading",
                wanted("Marrow Choir", "Silent Night", None).album(album.clone()),
                true,
            ),
            (
                "alice-music",
                "needs_decision",
                wanted("Marrow Choir", "Silent Night", None),
                false,
            ),
            (
                "bob-music",
                "downloading",
                wanted("Marrow Choir", "O Holy Night", None),
                false,
            ),
            // A job in an outer folder may import into alice's.
            (
                "alice-top",
                "importing",
                wanted("Marrow Choir", "O Holy Night", None),
                true,
            ),
            // A state JobState::parse does not know.
            (
                "alice-music",
                "paused",
                wanted("Marrow Choir", "Silent Night", None),
                true,
            ),
        ];
        for (i, (folder, state, query, blocks)) in cases.into_iter().enumerate() {
            let lib = Library::new().await;
            lib.folder("alice", "alice-top", "music", true).await;
            lib.fake.file(
                NativeSong {
                    mbz_recording_id: Some(mbid.into()),
                    ..lib.record("s1")
                },
                "al1",
            );
            lib.job("j1", folder, state, query).await;
            let verdict = lib.guard().check_song(&lib.alice(), "s1").await;
            match blocks {
                true => assert_eq!(refusal(verdict).0, FailureCode::DeleteInUse, "case {i}"),
                false => {
                    library_verdict(verdict);
                }
            }
        }
        // A `wanted` that does not parse.
        let lib = Library::new().await;
        sqlx::query(
            "INSERT INTO jobs (id, user_id, folder_id, created_at, updated_at, wanted, import_mode, state) \
             VALUES ('j2', 'alice', 'alice-music', 1763200000, 1763200000, '{\"tracks\":7}', 'album', 'queued')",
        )
        .execute(&lib.db)
        .await
        .unwrap();
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await).0,
            FailureCode::DeleteInUse
        );
    }

    #[tokio::test]
    async fn a_pending_discovery_track_is_handed_back_by_song_id_or_exact_path() {
        let lib = Library::new().await;
        lib.song(
            "s5",
            "alice/Discovery/Explore/Marrow Choir/03.flac",
            b"audio",
        );
        lib.pending_track(
            "t1",
            "alice-music",
            "Explore",
            &lib.root
                .join("music/alice/Discovery/Explore/Marrow Choir/03.flac"),
            None,
        )
        .await;
        assert!(
            matches!(lib.guard().check_song(&lib.alice(), "s5").await, Ok(Verdict::Discovery { track, rating: 0 }) if track.id == "t1")
        );
        lib.pending_track(
            "t2",
            "alice-music",
            "Explore",
            &lib.root.join("music/alice/elsewhere.flac"),
            Some("s1"),
        )
        .await;
        assert!(
            matches!(lib.guard().check_song(&lib.alice(), "s1").await, Ok(Verdict::Discovery { track, rating: 1 }) if track.id == "t2")
        );
        // By song id before any path: Navidrome's size for s6 does not match the file on disk.
        lib.song("s6", "alice/Discovery/Explore/06.flac", b"audio");
        lib.fake.file(
            NativeSong {
                size: 9,
                ..lib.record("s6")
            },
            "al1",
        );
        lib.pending_track(
            "t3",
            "alice-music",
            "Explore",
            &lib.root.join("music/alice/Discovery/Explore/06.flac"),
            Some("s6"),
        )
        .await;
        assert!(
            matches!(lib.guard().check_song(&lib.alice(), "s6").await, Ok(Verdict::Discovery { track, .. }) if track.id == "t3")
        );
        // Handing a track back neither removes nor marks it.
        for id in ["t1", "t2", "t3"] {
            assert_eq!(
                (lib.track_status(id).await, lib.history_outcome(id).await),
                ("Pending".to_string(), "suggested".to_string())
            );
        }
        assert_eq!(lib.fake.ratings_set(), vec![]);
    }

    #[tokio::test]
    async fn a_native_path_that_leaves_its_library_is_refused_even_when_a_file_is_there() {
        let lib = Library::new().await;
        std::fs::create_dir_all(lib.root.join("outside")).unwrap();
        std::fs::write(lib.root.join("outside/z.flac"), b"audio").unwrap();
        let absolute = lib
            .root
            .join("outside/z.flac")
            .to_string_lossy()
            .into_owned();
        for (id, path) in [("s6", "../outside/z.flac".to_string()), ("s7", absolute)] {
            lib.fake.file(
                NativeSong {
                    id: id.into(),
                    path: path.clone(),
                    size: 5,
                    ..lib.record("s1")
                },
                "al1",
            );
            lib.fake.sees("alice", id);
            assert_eq!(
                refusal(lib.guard().check_song(&lib.alice(), id).await),
                (
                    FailureCode::DeletePathUnresolved,
                    format!("Navidrome's path for this song is not inside its library: {path}")
                )
            );
        }
        assert!(lib.root.join("outside/z.flac").exists());
        assert_eq!(lib.fake.ratings_set(), vec![]);
    }

    #[tokio::test]
    async fn a_file_an_unresolvable_folder_of_another_user_may_hold_is_kept_without_naming_that_folder(
    ) {
        let lib = Library::new().await;
        // Stored with `..`: it neither resolves nor compares.
        lib.folder("bob", "bob-odd", "music/alice/../bob", false)
            .await;
        let f = lib
            .guard()
            .check_song(&lib.alice(), "s1")
            .await
            .unwrap_err();
        assert_eq!(
            (f.code, f.message, f.cause),
            (
                FailureCode::DeleteOutsideLibrary,
                format!(
                    "{} may be inside a folder of another user",
                    canonical(&lib, "music/alice/A/x.flac").display()
                ),
                None
            )
        );
        assert_eq!(lib.fake.ratings_set(), vec![]);
    }

    /// A second record of `music/alice/A/x.flac`: library 2 at `{root}/music/alice`.
    fn second_record(lib: &Library) -> NativeSong {
        NativeSong {
            id: "s1b".into(),
            library_id: 2,
            library_path: lib.root.join("music/alice").to_string_lossy().into_owned(),
            path: "A/x.flac".into(),
            ..lib.record("s1")
        }
    }

    #[tokio::test]
    async fn a_second_song_of_the_file_keeps_it_and_says_who_sees_it_only_to_its_viewer() {
        for viewer in ["bob", "alice"] {
            let lib = Library::new().await;
            lib.fake.file(second_record(&lib), "al1");
            lib.fake.sees(viewer, "s1b");
            let expected = match viewer {
                "bob" => (FailureCode::DeleteVetoUnknown, VETO.to_string()),
                _ => (
                    FailureCode::DeletePathUnresolved,
                    format!(
                        "Navidrome lists {} as more than one song",
                        canonical(&lib, "music/alice/A/x.flac").display()
                    ),
                ),
            };
            assert_eq!(
                refusal(lib.guard().check_song(&lib.alice(), "s1").await),
                expected
            );
            assert_eq!(lib.fake.ratings_set(), vec![]);
            assert!(lib.root.join("music/alice/A/x.flac").exists());
        }
    }

    #[tokio::test]
    async fn a_second_song_in_a_library_not_on_this_machine_may_be_the_file_and_keeps_it() {
        // Library 2 is a second Navidrome mount of alice's folder that Soulful does not have.
        for viewer in ["bob", "alice"] {
            let lib = Library::new().await;
            lib.fake.file(
                NativeSong {
                    library_path: lib.root.join("music-kids").to_string_lossy().into_owned(),
                    ..second_record(&lib)
                },
                "al1",
            );
            lib.fake.sees(viewer, "s1b");
            if viewer == "bob" {
                lib.fake.unsee("bob", "s1");
            }
            let expected = match viewer {
                "bob" => (FailureCode::DeleteVetoUnknown, VETO.to_string()),
                _ => (
                    FailureCode::DeletePathUnresolved,
                    format!(
                        "Navidrome may list {} as more than one song",
                        canonical(&lib, "music/alice/A/x.flac").display()
                    ),
                ),
            };
            assert_eq!(
                refusal(lib.guard().check_song(&lib.alice(), "s1").await),
                expected,
                "{viewer}"
            );
            assert_eq!(lib.fake.ratings_set(), vec![]);
        }
        // Of another size it is another file.
        let lib = Library::new().await;
        lib.fake.file(
            NativeSong {
                library_path: lib.root.join("music-kids").to_string_lossy().into_owned(),
                size: 9,
                ..second_record(&lib)
            },
            "al1",
        );
        lib.fake.sees("bob", "s1b");
        library_verdict(lib.guard().check_song(&lib.alice(), "s1").await);
    }

    #[tokio::test]
    async fn a_root_learned_from_another_file_cannot_delete_a_song_under_anothers_id() {
        // Dave's shared folder is Navidrome's whole library 1; Soulful learned alice's folder as
        // its root from dave's song "t" of the same size (the index lookup took it for hers).
        let lib = Library::new().await;
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ('dave', 'dave', 'x')")
            .execute(&lib.db)
            .await
            .unwrap();
        lib.folder("dave", "dave-music", "music", true).await;
        lib.write("A/x.flac", b"audio");
        lib.fake.file(
            NativeSong {
                id: "t".into(),
                path: "A/x.flac".into(),
                ..lib.record("s1")
            },
            "al1",
        );
        lib.fake.sees("alice", "t");
        lib.fake.rates("alice", "t", 1);
        crate::models::navidrome_library::NavidromeLibraryRow::upsert_in(
            &lib.db,
            &crate::models::navidrome_library::NavidromeLibraryRow {
                library_id: 1,
                navidrome_path: lib.record("s1").library_path,
                local_path: lib.root.join("music/alice").to_string_lossy().into_owned(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "t").await),
            (
                FailureCode::DeletePathUnresolved,
                format!(
                    "Navidrome may list {} as more than one song",
                    canonical(&lib, "music/alice/A/x.flac").display()
                )
            )
        );
        let item = ImportedItem {
            path: "A/x.flac".into(),
            recording_mbid: None,
            artist: "Marrow Choir".into(),
            title: "O Holy Night".into(),
            album: "Winter Songs".into(),
        };
        assert_eq!(
            lib.guard()
                .check_file(&lib.alice(), &item, &lib.root.join("music/alice"), &[])
                .await
                .unwrap_err()
                .code,
            FailureCode::DeletePathUnresolved
        );
        assert!(
            lib.root.join("music/alice/A/x.flac").exists()
                && lib.root.join("music/A/x.flac").exists()
        );
        assert_eq!(lib.fake.ratings_set(), vec![]);
    }

    #[tokio::test]
    async fn a_copy_navidrome_lists_before_the_imported_file_never_reaches_it() {
        // Dave's shared folder is Navidrome's whole library 1 and holds his own A/x.flac, of the
        // size of the A/x.flac alice imported into music/alice, written at another time.
        // Navidrome has not listed alice's file yet: the index lookup of her import finds only
        // dave's song "t", which she rated 1.
        let lib = Library::new().await;
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ('dave', 'dave', 'x')")
            .execute(&lib.db)
            .await
            .unwrap();
        lib.folder("dave", "dave-music", "music", true).await;
        lib.write("A/x.flac", b"audio");
        let written: chrono::DateTime<chrono::Utc> =
            "2026-10-03T21:14:09.583201447Z".parse().unwrap();
        test_support::set_mtime(&lib.root.join("music/A/x.flac"), written);
        lib.fake.file(
            NativeSong {
                id: "t".into(),
                path: "A/x.flac".into(),
                updated_at: written,
                ..lib.record("s1")
            },
            "al1",
        );
        lib.fake.unsee("alice", "s1");
        lib.fake.unsee("bob", "s1");
        lib.fake.sees("alice", "t");
        lib.fake.rates("alice", "t", 1);
        let imported = lib.root.join("music/alice/A/x.flac");
        assert_eq!(
            paths::learn_in(&lib.db, &[lib.record("t")], &imported)
                .await
                .unwrap(),
            Vec::<i64>::new()
        );
        // Unlearned, t's file is dave's own, outside alice's folders.
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "t").await),
            (
                FailureCode::DeleteOutsideLibrary,
                format!(
                    "{} is not inside one of your library folders",
                    canonical(&lib, "music/A/x.flac").display()
                )
            )
        );
        assert!(imported.exists() && lib.root.join("music/A/x.flac").exists());
        assert_eq!(lib.fake.ratings_set(), vec![]);
    }

    /// Library 1 as Navidrome mounts it, at a path this machine does not have.
    const NAVIDROME_MOUNT: &str = "/srv/navidrome/music";

    /// Song s1 as Navidrome lists it under NAVIDROME_MOUNT, its file modified at `updated_at`
    /// by Navidrome's reading.
    fn mounted_elsewhere(lib: &Library, updated_at: chrono::DateTime<chrono::Utc>) {
        lib.fake.file(
            NativeSong {
                library_path: NAVIDROME_MOUNT.into(),
                updated_at,
                ..lib.record("s1")
            },
            "al1",
        );
    }

    /// The root stored for library 1, if any.
    async fn learned_root(lib: &Library) -> Option<String> {
        crate::models::navidrome_library::NavidromeLibraryRow::get_in(&lib.db, 1)
            .await
            .unwrap()
            .map(|row| row.local_path)
    }

    #[tokio::test]
    async fn a_root_no_import_taught_is_learned_by_the_check_that_needs_it() {
        // Navidrome mounts the music elsewhere, and no import has taught where: the check
        // finds alice's file for s1 in her folder, proves it, learns the root and goes on.
        let lib = Library::new().await;
        mounted_elsewhere(&lib, test_support::mtime());
        lib.fake.rates("alice", "s1", 1);
        let checked = library_verdict(lib.guard().check_song(&lib.alice(), "s1").await);
        assert_eq!(checked.path, canonical(&lib, "music/alice/A/x.flac"));
        assert_eq!(
            learned_root(&lib).await,
            Some(lib.root.join("music").to_string_lossy().into_owned())
        );

        // A replacement's earlier copy teaches it the same way, from the item's own file.
        let lib = Library::new().await;
        mounted_elsewhere(&lib, test_support::mtime());
        lib.fake.rates("alice", "s1", 1);
        let item = ImportedItem {
            path: "A/x.flac".into(),
            recording_mbid: None,
            artist: "Marrow Choir".into(),
            title: "O Holy Night".into(),
            album: "Winter Songs".into(),
        };
        let checked = lib
            .guard()
            .check_file(&lib.alice(), &item, &lib.root.join("music/alice"), &[])
            .await
            .unwrap();
        assert_eq!(checked.path, canonical(&lib, "music/alice/A/x.flac"));
        assert_eq!(
            learned_root(&lib).await,
            Some(lib.root.join("music").to_string_lossy().into_owned())
        );
    }

    #[tokio::test]
    async fn a_root_the_file_cannot_prove_is_not_learned_and_the_check_refuses() {
        let no_file = (
            FailureCode::DeletePathUnresolved,
            format!("No file at {NAVIDROME_MOUNT}/alice/A/x.flac"),
        );
        // Navidrome read an earlier write of the file: another modification time.
        let earlier: chrono::DateTime<chrono::Utc> =
            "2026-10-03T21:14:09.583201447Z".parse().unwrap();
        let lib = Library::new().await;
        mounted_elsewhere(&lib, earlier);
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await),
            no_file
        );
        let item = ImportedItem {
            path: "A/x.flac".into(),
            recording_mbid: None,
            artist: "Marrow Choir".into(),
            title: "O Holy Night".into(),
            album: "Winter Songs".into(),
        };
        let f = lib
            .guard()
            .check_file(&lib.alice(), &item, &lib.root.join("music/alice"), &[])
            .await
            .unwrap_err();
        assert_eq!((f.code, f.message), no_file);
        assert_eq!(learned_root(&lib).await, None);

        // Two of alice's folders each hold a file that proves a root (a copy that kept its
        // times): either may be the song's, so neither is learned.
        let lib = Library::new().await;
        mounted_elsewhere(&lib, test_support::mtime());
        lib.folder("alice", "alice-backup", "backup/alice", false)
            .await;
        let copy = lib.root.join("backup/alice/A/x.flac");
        std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
        std::fs::write(&copy, b"audio").unwrap();
        test_support::set_mtime(&copy, test_support::mtime());
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await),
            no_file
        );
        assert_eq!(learned_root(&lib).await, None);
        assert_eq!(lib.fake.ratings_set(), vec![]);
    }

    /// The library item of song s1's file, A/x.flac with s1's tags.
    fn item_x() -> ImportedItem {
        ImportedItem {
            path: "A/x.flac".into(),
            recording_mbid: None,
            artist: "Marrow Choir".into(),
            title: "O Holy Night".into(),
            album: "Winter Songs".into(),
        }
    }

    /// Alice's folder alice-mirror at `{root}/mirror/alice`, a copy of music/alice that kept
    /// its times (rsync -a, cp -p) holding A/x.flac and A/y.flac; song s2 at
    /// `{root}/music/alice/A/y.flac`, rated 1 by alice; and s1's file under music gone.
    async fn mirrored_with_one_file_gone(lib: &Library) {
        lib.folder("alice", "alice-mirror", "mirror/alice", false)
            .await;
        lib.song("s2", "alice/A/y.flac", b"other!");
        lib.fake.rates("alice", "s2", 1);
        for (rel, bytes) in [("A/x.flac", &b"audio"[..]), ("A/y.flac", &b"other!"[..])] {
            let copy = lib.root.join("mirror/alice").join(rel);
            std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
            std::fs::write(&copy, bytes).unwrap();
            test_support::set_mtime(&copy, test_support::mtime());
        }
        std::fs::remove_file(lib.root.join("music/alice/A/x.flac")).unwrap();
    }

    /// Checks s1 by song and by its item in the mirror, both refused for the file gone under
    /// the library's root, then s2, which must still be its file under music.
    async fn only_the_gone_file_is_refused(lib: &Library) {
        let no_file = (
            FailureCode::DeletePathUnresolved,
            format!(
                "No file at {}",
                lib.root.join("music/alice/A/x.flac").display()
            ),
        );
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await),
            no_file
        );
        let f = lib
            .guard()
            .check_file(&lib.alice(), &item_x(), &lib.root.join("mirror/alice"), &[])
            .await
            .unwrap_err();
        assert_eq!((f.code, f.message), no_file);
        assert_eq!(lib.fake.ratings_set(), vec![]);
        let checked = library_verdict(lib.guard().check_song(&lib.alice(), "s2").await);
        assert_eq!(checked.path, canonical(lib, "music/alice/A/y.flac"));
        assert!(
            lib.root.join("mirror/alice/A/x.flac").exists()
                && lib.root.join("mirror/alice/A/y.flac").exists()
        );
    }

    #[tokio::test]
    async fn a_file_gone_under_navidromes_own_root_teaches_no_root_from_a_mirror() {
        // Navidrome's library path is here, so it is the root: the mirror's copy of the gone
        // file proves another root, which must not become the library's.
        let lib = Library::new().await;
        mirrored_with_one_file_gone(&lib).await;
        only_the_gone_file_is_refused(&lib).await;
        assert_eq!(learned_root(&lib).await, None);
    }

    #[tokio::test]
    async fn a_file_gone_under_a_stored_root_keeps_that_root() {
        // Navidrome mounts the music elsewhere and an import taught the root: one file gone
        // under it neither replaces nor forgets it.
        let lib = Library::new().await;
        mounted_elsewhere(&lib, test_support::mtime());
        let music = lib.root.join("music").to_string_lossy().into_owned();
        crate::models::navidrome_library::NavidromeLibraryRow::upsert_in(
            &lib.db,
            &crate::models::navidrome_library::NavidromeLibraryRow {
                library_id: 1,
                navidrome_path: NAVIDROME_MOUNT.into(),
                local_path: music.clone(),
            },
        )
        .await
        .unwrap();
        mirrored_with_one_file_gone(&lib).await;
        only_the_gone_file_is_refused(&lib).await;
        assert_eq!(learned_root(&lib).await, Some(music));
    }

    #[tokio::test]
    async fn a_stored_root_that_is_gone_is_replaced_by_the_root_a_check_proves() {
        let lib = Library::new().await;
        mounted_elsewhere(&lib, test_support::mtime());
        crate::models::navidrome_library::NavidromeLibraryRow::upsert_in(
            &lib.db,
            &crate::models::navidrome_library::NavidromeLibraryRow {
                library_id: 1,
                navidrome_path: NAVIDROME_MOUNT.into(),
                local_path: lib.root.join("old-mount").to_string_lossy().into_owned(),
            },
        )
        .await
        .unwrap();
        let checked = library_verdict(lib.guard().check_song(&lib.alice(), "s1").await);
        assert_eq!(checked.path, canonical(&lib, "music/alice/A/x.flac"));
        assert_eq!(
            learned_root(&lib).await,
            Some(lib.root.join("music").to_string_lossy().into_owned())
        );
    }

    #[tokio::test]
    async fn a_folder_that_cannot_be_read_may_hold_a_second_copy_so_no_root_is_learned() {
        use std::os::unix::fs::PermissionsExt;
        // With no root here, alice's music/alice proves one; her backup/alice cannot be
        // searched, so a second copy may be there.
        let lib = Library::new().await;
        mounted_elsewhere(&lib, test_support::mtime());
        lib.folder("alice", "alice-backup", "backup/alice", false)
            .await;
        let locked = lib.root.join("backup/alice/A");
        std::fs::create_dir_all(&locked).unwrap();
        let unreadable = std::fs::canonicalize(&locked).unwrap().join("x.flac");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let checked = lib.guard().check_song(&lib.alice(), "s1").await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let f = match checked {
            Err(f) => f,
            Ok(_) => panic!("allowed"),
        };
        assert_eq!(
            (f.code, f.message.as_str()),
            (
                FailureCode::DeletePathUnresolved,
                format!("No file at {NAVIDROME_MOUNT}/alice/A/x.flac").as_str()
            )
        );
        assert_eq!(
            f.cause,
            Some(Cause::new(
                Service::Filesystem,
                None,
                format!(
                    "{}: {}",
                    unreadable.display(),
                    std::io::Error::from_raw_os_error(13) // EACCES
                )
            ))
        );
        assert_eq!(learned_root(&lib).await, None);
        assert_eq!(lib.fake.ratings_set(), vec![]);

        // A folder that holds nothing at the song's path is no second copy.
        let checked = library_verdict(lib.guard().check_song(&lib.alice(), "s1").await);
        assert_eq!(checked.path, canonical(&lib, "music/alice/A/x.flac"));
        assert_eq!(
            learned_root(&lib).await,
            Some(lib.root.join("music").to_string_lossy().into_owned())
        );
    }

    #[tokio::test]
    async fn another_users_song_that_links_to_the_file_is_the_same_file() {
        // Navidrome follows links and lists bob's link as a song of its own.
        let lib = Library::new().await;
        std::os::unix::fs::symlink(
            lib.root.join("music/alice/A/x.flac"),
            lib.root.join("music/bob/x.flac"),
        )
        .unwrap();
        lib.fake.file(
            NativeSong {
                id: "s8".into(),
                path: "bob/x.flac".into(),
                ..lib.record("s1")
            },
            "al1",
        );
        lib.fake.sees("bob", "s8");
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await),
            (FailureCode::DeleteVetoUnknown, VETO.into())
        );
        assert_eq!(lib.fake.ratings_set(), vec![]);
    }

    #[tokio::test]
    async fn a_song_its_own_search_does_not_find_cannot_be_checked() {
        let lib = Library::new().await;
        lib.fake.unsearchable("s1");
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await),
            (
                FailureCode::DeletePathUnresolved,
                format!(
                    "Navidrome cannot be searched for the tags of {}",
                    canonical(&lib, "music/alice/A/x.flac").display()
                )
            )
        );
        assert_eq!(lib.fake.ratings_set(), vec![]);
    }

    #[tokio::test]
    async fn navidrome_before_0_60_or_of_an_unknown_version_keeps_every_file() {
        for version in [Some("0.59.0 (fa46aefa)"), Some("dev"), None] {
            let lib = Library::new().await;
            lib.fake.reports_version(version);
            assert_eq!(
                refusal(lib.guard().check_song(&lib.alice(), "s1").await),
                (
                    FailureCode::DeleteVetoUnknown,
                    format!(
                        "Navidrome 0.60 or newer is needed to see other listeners' ratings (this server reports {})",
                        version.unwrap_or("no version")
                    )
                )
            );
            assert_eq!(lib.fake.ratings_set(), vec![]);
        }
        let lib = Library::new().await;
        lib.fake
            .version_unreadable(|| NavidromeLookupError::Client(NavidromeError::CircuitOpen));
        let f = lib
            .guard()
            .check_song(&lib.alice(), "s1")
            .await
            .unwrap_err();
        assert_eq!(
            (f.code, f.message.as_str(), f.cause),
            (
                FailureCode::NavidromeCircuitOpen,
                "Soulful could not read your Navidrome server's version, which this deletion check needs",
                Some(Cause::new(
                    Service::Navidrome,
                    None,
                    NavidromeError::CircuitOpen.to_string()
                ))
            )
        );
        assert_eq!(lib.fake.ratings_set(), vec![]);
    }

    #[tokio::test]
    async fn a_file_nobody_rated_has_no_average_and_passes_rule_four() {
        let lib = Library::new().await;
        lib.song("s2", "alice/A/y.flac", b"audio");
        let checked = library_verdict(lib.guard().check_song(&lib.alice(), "s2").await);
        assert_eq!(
            (checked.song.rating, checked.song.average_rating),
            (0, None)
        );
        assert_eq!(
            lib.fake.ratings_set(),
            vec![("alice".to_string(), "s2".to_string(), 0)]
        );
    }

    #[tokio::test]
    async fn an_own_rating_with_no_average_means_navidrome_sends_none() {
        let lib = Library::new().await;
        lib.fake.without_averages();
        lib.fake.rates("alice", "s1", 3);
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await),
            (FailureCode::DeleteVetoUnknown, VETO.into())
        );
        assert_eq!(
            lib.fake.ratings_set(),
            vec![("alice".to_string(), "s1".to_string(), 3)]
        );
    }

    #[tokio::test]
    async fn a_rating_changed_during_the_check_is_the_one_re_sent_and_handed_back() {
        let lib = Library::new().await;
        lib.fake.rates("alice", "s1", 1);
        let stale = lib
            .fake
            .native_song(&lib.alice(), "s1")
            .await
            .unwrap()
            .unwrap();
        let (path, id) = paths::local_file_in(&lib.db, &stale, None).await.unwrap();
        lib.fake.rates("alice", "s1", 5);
        // A rating above 1 needs Navidrome's user list to rule out a rater Soulful cannot read.
        lib.fake.admin("bob");
        let checked = lib
            .guard()
            .check_resolved(&lib.alice(), stale, path, id, &[])
            .await
            .unwrap();
        assert_eq!(
            lib.fake.ratings_set(),
            vec![("alice".to_string(), "s1".to_string(), 5)]
        );
        assert_eq!(
            (checked.song.rating, checked.song.average_rating),
            (5, Some(5.0))
        );
    }

    #[tokio::test]
    async fn an_average_above_the_callers_own_share_blocks() {
        let lib = Library::new().await;
        lib.fake.rates("alice", "s1", 3);
        lib.fake.hidden_rater("s1", 5);
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await).0,
            FailureCode::DeleteBlockedByOtherUser
        );
    }

    #[tokio::test]
    async fn a_checked_file_is_deleted_recorded_forgotten_and_its_empty_folders_pruned() {
        let lib = Library::new().await;
        let stub = StubImporter::new();
        let checked = checked(&lib).await;
        let (path, folder) = (checked.path.clone(), checked.folder.clone());
        let warnings = lib
            .guard()
            .carry_out(
                &lib.alice(),
                checked,
                DeletionReason::Disliked,
                Origin::Auto,
                ForgetVia::Worker {
                    importer: &stub,
                    library: &folder,
                },
            )
            .await
            .unwrap();
        assert_eq!(warnings, Vec::<String>::new());
        assert!(!path.exists() && !folder.join("A").exists() && folder.is_dir());
        assert_eq!(stub.forgets(), vec![(folder.clone(), path.clone())]);
        let rows: Vec<(String, String, String, String, String)> =
            sqlx::query_as("SELECT user_id, song_id, path, origin, reason FROM deletion_reviews")
                .fetch_all(&lib.db)
                .await
                .unwrap();
        assert_eq!(
            rows,
            vec![(
                "alice".into(),
                "s1".into(),
                path.to_string_lossy().into_owned(),
                "auto".into(),
                "disliked".into()
            )]
        );
    }

    #[tokio::test]
    async fn a_forget_queued_on_the_folders_worker_runs_after_the_request_and_prunes() {
        let lib = Library::new().await;
        let stub = Arc::new(StubImporter::new());
        let queue = ImportQueue::new(RecordingDeps::new(stub.clone()));
        let checked = checked(&lib).await;
        let (path, folder) = (checked.path.clone(), checked.folder.clone());
        let warnings = lib
            .guard()
            .carry_out(
                &lib.alice(),
                checked,
                DeletionReason::Disliked,
                Origin::Auto,
                ForgetVia::Queue(queue.clone()),
            )
            .await
            .unwrap();
        assert!(warnings.is_empty() && !path.exists());
        drop(queue); // the spawned task holds its own handle
        tokio::time::timeout(Duration::from_secs(10), async {
            while stub.forgets().is_empty() || folder.join("A").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the queued forget never ran");
        assert_eq!(stub.forgets(), vec![(folder.clone(), path.clone())]);
    }

    #[tokio::test]
    async fn a_request_dropped_after_the_unlink_still_forgets_and_prunes() {
        let lib = Library::new().await;
        let stub = Arc::new(StubImporter::new());
        let queue = ImportQueue::new(RecordingDeps::new(stub.clone()));
        let checked = checked(&lib).await;
        let (path, folder) = (checked.path.clone(), checked.folder.clone());
        let guard = lib.guard();
        let alice = lib.alice();
        let mut request = Box::pin(guard.carry_out(
            &alice,
            checked,
            DeletionReason::Disliked,
            Origin::Auto,
            ForgetVia::Queue(queue),
        ));
        // The test runtime has one thread: the removal task only runs while this task sleeps,
        // so the request is never polled again once the file is gone, and is dropped there.
        while path.exists() {
            let poll = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(std::future::Future::poll(request.as_mut(), cx))
            });
            assert!(
                poll.await.is_pending(),
                "carry_out finished before the unlink"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        drop(request);
        tokio::time::timeout(Duration::from_secs(10), async {
            while stub.forgets().is_empty() || folder.join("A").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the forget of a dropped request never ran");
        assert_eq!(stub.forgets(), vec![(folder.clone(), path.clone())]);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM deletion_reviews")
                .fetch_one(&lib.db)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn a_path_component_swapped_for_a_symlink_is_not_followed() {
        let lib = Library::new().await;
        let checked = checked(&lib).await;
        let album = lib.root.join("music/alice/A");
        std::fs::rename(&album, lib.root.join("music/alice/A.moved")).unwrap();
        std::fs::create_dir_all(lib.root.join("elsewhere")).unwrap();
        // The same inode, so the identity matches through the link: only a no-follow open refuses.
        std::fs::hard_link(
            lib.root.join("music/alice/A.moved/x.flac"),
            lib.root.join("elsewhere/x.flac"),
        )
        .unwrap();
        std::os::unix::fs::symlink(lib.root.join("elsewhere"), &album).unwrap();
        let stub = StubImporter::new();
        let f = lib
            .guard()
            .carry_out(
                &lib.alice(),
                checked,
                DeletionReason::Disliked,
                Origin::Auto,
                ForgetVia::Worker {
                    importer: &stub,
                    library: &lib.root.join("music/alice"),
                },
            )
            .await
            .unwrap_err()
            .into_failure();
        assert_eq!(
            (f.code, f.message.as_str()),
            (
                FailureCode::DeleteChangedSincePreview,
                "The file changed after it was checked"
            )
        );
        assert!(
            lib.root.join("elsewhere/x.flac").exists()
                && lib.root.join("music/alice/A.moved/x.flac").exists()
        );
        assert!(stub.forgets().is_empty());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM deletion_reviews")
                .fetch_one(&lib.db)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn a_file_rewritten_since_the_check_is_left() {
        let lib = Library::new().await;
        let checked = checked(&lib).await;
        let path = checked.path.clone();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"other audio").unwrap();
        let stub = StubImporter::new();
        let f = lib
            .guard()
            .carry_out(
                &lib.alice(),
                checked,
                DeletionReason::Disliked,
                Origin::Auto,
                ForgetVia::Worker {
                    importer: &stub,
                    library: &lib.root.join("music/alice"),
                },
            )
            .await
            .unwrap_err()
            .into_failure();
        assert_eq!(f.code, FailureCode::DeleteChangedSincePreview);
        assert_eq!(std::fs::read(&path).unwrap(), b"other audio");
    }

    #[tokio::test]
    async fn an_unlink_the_os_refuses_is_a_failure_with_its_error() {
        use std::os::unix::fs::PermissionsExt;
        let lib = Library::new().await;
        let checked = checked(&lib).await;
        let dir = checked.path.parent().unwrap().to_path_buf();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let stub = StubImporter::new();
        let f = lib
            .guard()
            .carry_out(
                &lib.alice(),
                checked,
                DeletionReason::Disliked,
                Origin::Auto,
                ForgetVia::Worker {
                    importer: &stub,
                    library: &lib.root.join("music/alice"),
                },
            )
            .await
            .unwrap_err()
            .into_failure();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let os = std::io::Error::from_raw_os_error(13); // EACCES, "Permission denied (os error 13)"
        assert_eq!(
            (f.code, f.retry),
            (FailureCode::DeleteFailed, RetryAdvice::SameSourceLater)
        );
        assert_eq!(
            f.cause,
            Some(Cause::new(Service::Filesystem, None, os.to_string()))
        );
    }

    #[tokio::test]
    async fn a_removal_that_lost_track_of_itself_leaves_the_audit_as_the_file_is() {
        let lib = Library::new().await;
        let Checked { song, path, id, .. } = checked(&lib).await;
        let record = record_of(
            &lib.alice(),
            song,
            &path,
            DeletionReason::Disliked,
            Origin::Auto,
        );
        let rows = || async {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM deletion_reviews")
                .fetch_one(&lib.db)
                .await
                .unwrap()
        };
        // Still there, rewritten in place since: it was kept, and nothing is recorded.
        std::fs::write(&path, b"other audio").unwrap();
        match lost_removal(&lib.db, &record, &path, id, "task 7 panicked").await {
            Err(NotRemoved::Kept(f)) => assert_eq!(
                (f.code, f.message),
                (
                    FailureCode::DeleteFailed,
                    format!("Could not delete {}: task 7 panicked", path.display())
                )
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(rows().await, 0);
        assert!(path.exists());

        // Its folder now a link: it cannot be looked at without following one.
        let album = path.parent().unwrap().to_path_buf();
        let moved = album.with_file_name("A.moved");
        std::fs::rename(&album, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &album).unwrap();
        match lost_removal(&lib.db, &record, &path, id, "task 7 panicked").await {
            Err(NotRemoved::Unconfirmed(f)) => assert_eq!(
                (f.code, f.message),
                (
                    FailureCode::DeleteFailed,
                    format!(
                        "{} may have been deleted: the removal stopped before it reported (task 7 panicked)",
                        path.display()
                    )
                )
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(rows().await, 0);

        // Another file at the path: the checked one is gone, so the removal is done and
        // recorded, but beets' item at the path may be the new file's.
        std::fs::remove_file(&album).unwrap();
        std::fs::remove_dir_all(&moved).unwrap();
        std::fs::create_dir(&album).unwrap();
        std::fs::write(&path, b"a new file").unwrap();
        assert!(matches!(
            lost_removal(&lib.db, &record, &path, id, "task 7 panicked").await,
            Ok((None, Left::AnotherFile))
        ));
        assert_eq!(rows().await, 1);

        // Nothing at the path: the removal is done. The task and its caller may both settle
        // it: the row is recorded once.
        std::fs::remove_file(&path).unwrap();
        for _ in 0..2 {
            assert!(matches!(
                lost_removal(&lib.db, &record, &path, id, "task 7 panicked").await,
                Ok((None, Left::Nothing))
            ));
        }
        assert_eq!(rows().await, 1);
    }

    #[tokio::test]
    async fn a_removal_forgets_and_prunes_only_when_nothing_is_left_at_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("alice");
        let path = folder.join("A/x.flac");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        // Another file at the path: its beets item stays, and so does its folder.
        std::fs::write(&path, b"a new file").unwrap();
        let stub = StubImporter::new();
        assert_eq!(
            forget_in_worker(&stub, &folder, &path, &folder, Left::AnotherFile).await,
            None
        );
        assert!(stub.forgets().is_empty());
        assert!(path.exists());

        // Nothing there: the item is forgotten and the empty folder pruned.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            forget_in_worker(&stub, &folder, &path, &folder, Left::Nothing).await,
            None
        );
        assert_eq!(stub.forgets(), vec![(folder.clone(), path.clone())]);
        assert!(!folder.join("A").exists());
        assert!(folder.is_dir());
    }

    #[tokio::test]
    async fn an_empty_folder_that_cannot_be_removed_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("alice");
        std::fs::create_dir_all(folder.join("A")).unwrap();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).unwrap();
        let pruning = pruned(&folder.join("A/x.flac"), &folder).await;
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            pruning,
            Err(format!(
                "Could not remove the empty folders above {} up to {}: {}",
                folder.join("A/x.flac").display(),
                folder.display(),
                std::io::Error::from_raw_os_error(13) // EACCES
            ))
        );
        assert!(folder.join("A").is_dir());
    }

    #[test]
    fn a_forget_that_did_not_remove_one_item_is_a_warning() {
        let (p, l) = (
            Path::new("/music/alice/A/x.flac"),
            Path::new("/music/alice"),
        );
        assert_eq!(forget_note(&ForgetOutcome::Removed, p, l), None);
        assert_eq!(
            forget_note(&ForgetOutcome::NotInLibrary, p, l).as_deref(),
            Some("/music/alice/A/x.flac was not in the library of /music/alice")
        );
        assert_eq!(
            forget_note(&ForgetOutcome::Several(2), p, l).as_deref(),
            Some(
                "Left /music/alice/A/x.flac in the library of /music/alice: 2 items have that path"
            )
        );
        // import_queue/mod.rs:33 WORKER_STOPPED, what ImportQueue::forget reports for a worker that went away.
        assert_eq!(
            forget_note(&ForgetOutcome::Failed("import worker stopped".into()), p, l).as_deref(),
            Some("Could not remove /music/alice/A/x.flac from the library of /music/alice: import worker stopped")
        );
    }

    #[test]
    fn guard_refusals_keep_their_text() {
        use FailureCode::*;
        use RetryAdvice::*;
        for (f, code, stage, retry, message) in [
            (
                song_not_found(),
                UnknownHandle,
                Stage::Input,
                NotRetryable,
                "Song not found",
            ),
            (
                blocked_by_other_user(),
                DeleteBlockedByOtherUser,
                Stage::Delete,
                NotRetryable,
                "Another listener rated or starred this track",
            ),
            (
                veto_unknown(),
                DeleteVetoUnknown,
                Stage::Delete,
                SameSourceLater,
                VETO,
            ),
            (
                in_use(),
                DeleteInUse,
                Stage::Delete,
                SameSourceLater,
                "A download or import that is not finished wants this track",
            ),
            (
                changed_since_check(),
                DeleteChangedSincePreview,
                Stage::Delete,
                SameSourceLater,
                "The file changed after it was checked",
            ),
            (
                // EACCES
                delete_failed(
                    Path::new("/music/alice/A/x.flac"),
                    &std::io::Error::from_raw_os_error(13),
                ),
                DeleteFailed,
                Stage::Delete,
                SameSourceLater,
                "Could not delete /music/alice/A/x.flac: Permission denied (os error 13)",
            ),
            (
                removal_unconfirmed(Path::new("/music/alice/A/x.flac"), "task 7 panicked"),
                DeleteFailed,
                Stage::Delete,
                SameSourceLater,
                "/music/alice/A/x.flac may have been deleted: the removal stopped before it reported (task 7 panicked)",
            ),
        ] {
            assert_eq!(
                (f.code, f.stage, f.retry, f.message.as_str()),
                (code, stage, retry, message)
            );
            assert_eq!(f.user_action, None);
        }
    }

    #[tokio::test]
    async fn a_reported_file_is_checked_as_the_song_whose_file_it_is() {
        let lib = Library::new().await;
        // Found first by the same search, and another file.
        lib.song("s0", "alice/B/w.flac", b"audio");
        let library = lib.root.join("music/alice");
        let item = |rel: &str| ImportedItem {
            path: rel.into(),
            recording_mbid: None,
            artist: "Marrow Choir".into(),
            title: "O Holy Night".into(),
            album: "Winter Songs".into(),
        };
        let checked = lib
            .guard()
            .check_file(&lib.alice(), &item("A/x.flac"), &library, &[])
            .await
            .unwrap();
        assert_eq!(
            (checked.song.id.as_str(), checked.path),
            ("s1", canonical(&lib, "music/alice/A/x.flac"))
        );

        let refused = |r: Result<Checked, Failure>| match r {
            Err(f) => (f.code, f.message),
            Ok(c) => panic!("allowed {c:?}"),
        };
        assert_eq!(
            refused(
                lib.guard()
                    .check_file(&lib.alice(), &item("A/gone.flac"), &library, &[])
                    .await
            ),
            (
                FailureCode::DeletePathUnresolved,
                format!("No file at {}", library.join("A/gone.flac").display())
            )
        );
        std::fs::write(library.join("A/z.flac"), b"audio").unwrap();
        assert_eq!(
            refused(
                lib.guard()
                    .check_file(&lib.alice(), &item("A/z.flac"), &library, &[])
                    .await
            ),
            (
                FailureCode::DeletePathUnresolved,
                format!(
                    "Navidrome has no song for {}",
                    canonical(&lib, "music/alice/A/z.flac").display()
                )
            )
        );
    }

    #[tokio::test]
    async fn the_files_own_song_says_why_it_cannot_be_checked() {
        let item = ImportedItem {
            path: "A/x.flac".into(),
            recording_mbid: None,
            artist: "Marrow Choir".into(),
            title: "O Holy Night".into(),
            album: "Winter Songs".into(),
        };
        let lib = Library::new().await;
        let library = lib.root.join("music/alice");
        let x = canonical(&lib, "music/alice/A/x.flac");
        let s1 = lib.record("s1");
        for (record, expected) in [
            (
                NativeSong {
                    size: 9,
                    ..s1.clone()
                },
                format!("{} has 5 bytes, Navidrome's song has 9", x.display()),
            ),
            (
                NativeSong {
                    missing: true,
                    ..s1.clone()
                },
                "Navidrome lists the file of this song as missing".to_string(),
            ),
        ] {
            lib.fake.file(record, "al1");
            let f = lib
                .guard()
                .check_file(&lib.alice(), &item, &library, &[])
                .await
                .unwrap_err();
            assert_eq!(
                (f.code, f.message),
                (FailureCode::DeletePathUnresolved, expected)
            );
        }
        // A song taken for the item whose library is not on this machine, and whose root the
        // item's file cannot prove (Navidrome read another modification time), says so.
        lib.fake.file(
            NativeSong {
                library_path: lib.root.join("elsewhere").to_string_lossy().into_owned(),
                updated_at: "2026-10-03T21:14:09.583201447Z".parse().unwrap(),
                ..s1
            },
            "al1",
        );
        let f = lib
            .guard()
            .check_file(&lib.alice(), &item, &library, &[])
            .await
            .unwrap_err();
        assert_eq!(
            f.message,
            format!(
                "No file at {}",
                lib.root.join("elsewhere/alice/A/x.flac").display()
            )
        );
        assert!(x.exists());
    }
}
