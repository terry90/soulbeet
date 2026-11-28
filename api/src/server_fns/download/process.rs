use dioxus::logger::tracing::{info, warn};
use shared::download::DownloadProgress;
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage, UserAction};
use soulbeet::{DriverChoice, ImportMode};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::utils::{file_id, resolve_adopted, resolve_in_destination, FileId, Unresolved};
use crate::auth::UserId;
use crate::import_queue::{Duplicates, ImportJob, JobOwner};
use crate::ledger::DuplicateRule;

/// Maximum number of retries when waiting for a downloaded file to appear on disk.
/// With exponential backoff (500ms, 1s, 2s, 4s, 8s), this covers ~15.5s total.
const FILE_RESOLVE_MAX_RETRIES: u32 = 5;

/// Initial delay between file resolution retries.
const FILE_RESOLVE_INITIAL_DELAY_MS: u64 = 500;

/// Find a finished download on disk, retrying with exponential backoff:
/// slskd can report completion before the file is visible through a shared
/// volume mount. `destination` is the batch folder the file was enqueued
/// into; `None` (a transfer this batch adopted) is looked for where slskd
/// saves a transfer without a batch folder (`resolve_adopted`). Files in `claimed` were already
/// handed off and are never returned; the file found is added to it. The
/// lock is held only while one attempt looks, never across a wait. When no
/// attempt finds one, the last says why.
pub async fn resolve_on_disk(
    filename: &str,
    base: &Path,
    destination: Option<&str>,
    claimed: &Mutex<HashSet<FileId>>,
) -> Result<PathBuf, Unresolved> {
    let mut why = match find_on_disk_async(filename, base, destination, claimed).await {
        Ok(path) => return Ok(path),
        Err(why) => why,
    };

    let mut delay_ms = FILE_RESOLVE_INITIAL_DELAY_MS;
    for retry in 1..=FILE_RESOLVE_MAX_RETRIES {
        warn!(
            "No file to hand off ({:?}, attempt {}), retrying in {}ms: {}",
            why, retry, delay_ms, filename
        );
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;

        match find_on_disk_async(filename, base, destination, claimed).await {
            Ok(path) => {
                info!(
                    "File appeared on disk after {}ms wait: {}",
                    delay_ms, filename
                );
                return Ok(path);
            }
            Err(again) => why = again,
        }

        delay_ms *= 2;
    }

    Err(why)
}

/// Asynchronous wrapper around `find_on_disk` that runs filesystem discovery on the blocking
/// threadpool without holding the `claimed` mutex across disk I/O.
pub async fn find_on_disk_async(
    filename: &str,
    base: &Path,
    destination: Option<&str>,
    claimed: &Mutex<HashSet<FileId>>,
) -> Result<PathBuf, Unresolved> {
    // Retry once if another concurrent slot claimed the same FileId while we were scanning disk.
    for _ in 0..4 {
        let exclude = claimed.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let filename_owned = filename.to_string();
        let base_owned = base.to_path_buf();
        let dest_owned = destination.map(str::to_string);
        let found = tokio::task::spawn_blocking(move || {
            let path = match dest_owned.as_deref() {
                Some(dest) => resolve_in_destination(&filename_owned, &base_owned, dest, &exclude)?,
                None => resolve_adopted(&filename_owned, &base_owned, &exclude)?,
            };
            let id = file_id(&path).ok_or(Unresolved::NotOnDisk)?;
            Ok::<(PathBuf, FileId), Unresolved>((path, id))
        })
        .await
        .map_err(|_| Unresolved::NotOnDisk)??;

        let mut guard = claimed.lock().unwrap_or_else(|e| e.into_inner());
        if guard.insert(found.1) {
            return Ok(found.0);
        }
    }
    Err(Unresolved::Claimed)
}

/// One look of `resolve_on_disk`, without waiting for the file to appear.
pub fn find_on_disk(
    filename: &str,
    base: &Path,
    destination: Option<&str>,
    claimed: &Mutex<HashSet<FileId>>,
) -> Result<PathBuf, Unresolved> {
    let mut claimed = claimed.lock().unwrap_or_else(|e| e.into_inner());
    let path = match destination {
        Some(destination) => resolve_in_destination(filename, base, destination, &claimed)?,
        None => resolve_adopted(filename, base, &claimed)?,
    };
    let id = file_id(&path).ok_or(Unresolved::NotOnDisk)?;
    match claimed.insert(id) {
        true => Ok(path),
        false => Err(Unresolved::Claimed),
    }
}

/// Row text for a finished download whose file was not found on disk.
const UNRESOLVED: &str = "Could not resolve file path";

/// The Failed row of a finished download whose file `resolve_on_disk` did
/// not find under `base`, the download folder, for the reason `why`.
///
/// The backend reported the file complete and `resolve_on_disk` already
/// waited out a slow mount. With no file by its name there, the backend
/// saves somewhere else than `base` shows, or Soulful cannot read there: a
/// person fixes that. With every such file handed off for another download
/// of the batch, Soulful matched the files wrongly, which no folder setting
/// fixes. Downloading it again ends the same way in both cases.
pub fn unresolved_row(row: DownloadProgress, base: &Path, why: Unresolved) -> DownloadProgress {
    let failure = Failure::new(
        FailureCode::FileNotFoundAfterDownload,
        Stage::FileLookup,
        RetryAdvice::NotRetryable,
        UNRESOLVED,
    );
    let failure = match why {
        Unresolved::NotOnDisk => failure.with_user_action(UserAction::fix_download_path(base)),
        Unresolved::Claimed => failure,
    };
    row.failed_with(failure)
}

/// A finished slot whose file was found on disk. `destination` is the batch
/// folder it was enqueued into, `None` for an adopted transfer.
pub struct ResolvedSlot {
    pub row: DownloadProgress,
    pub path: PathBuf,
    pub destination: Option<String>,
}

/// What a finished download gave to import.
pub enum Downloaded {
    /// One file, imported on its own.
    File(PathBuf),
    /// An album's folder, imported whole, and the files its rows downloaded into it: the
    /// folder may hold other downloads' files too.
    Album {
        folder: PathBuf,
        files: Vec<PathBuf>,
    },
}

/// An import of `downloaded` reporting to `user` through `rows`, which are
/// already stamped with their batch, treating earlier copies by the batch's
/// `duplicates` rule.
pub fn import_job(
    downloaded: Downloaded,
    rows: Vec<DownloadProgress>,
    target: &Path,
    user: &UserId,
    duplicates: &DuplicateRule,
    base: &Path,
) -> ImportJob {
    let (sources, files, mode) = match downloaded {
        Downloaded::File(file) => (vec![file.clone()], vec![file], ImportMode::Singleton),
        Downloaded::Album { folder, files } => (vec![folder], files, ImportMode::Album),
    };
    ImportJob {
        sources,
        files,
        target: target.to_path_buf(),
        mode,
        owner: Some(JobOwner {
            user_id: user.clone(),
            rows,
        }),
        snapshot_target: false,
        cleanup_root: Some(base.to_path_buf()),
        choice: DriverChoice::Auto,
        choices: None,
        duplicates: Duplicates::of(duplicates, user),
        last_run: None,
        lock_retries: 0,
    }
}

/// The imports of an album batch: its folder as one album, and each file
/// found outside it (an adopted transfer) on its own, since that file's
/// folder may hold anything.
pub fn album_jobs(
    resolved: Vec<ResolvedSlot>,
    target: &Path,
    user: &UserId,
    duplicates: &DuplicateRule,
    base: &Path,
) -> Vec<ImportJob> {
    // Each folder, its rows and their files.
    type Album = (PathBuf, Vec<DownloadProgress>, Vec<PathBuf>);
    let mut albums: Vec<Album> = Vec::new();
    let mut jobs = Vec::new();
    for slot in resolved {
        match slot.destination {
            Some(destination) => {
                let folder = base.join(destination);
                match albums.iter_mut().find(|(f, _, _)| *f == folder) {
                    Some((_, rows, files)) => {
                        rows.push(slot.row);
                        files.push(slot.path);
                    }
                    None => albums.push((folder, vec![slot.row], vec![slot.path])),
                }
            }
            None => jobs.push(import_job(
                Downloaded::File(slot.path),
                vec![slot.row],
                target,
                user,
                duplicates,
                base,
            )),
        }
    }
    let album_jobs = albums.into_iter().map(|(folder, rows, files)| {
        import_job(
            Downloaded::Album { folder, files },
            rows,
            target,
            user,
            duplicates,
            base,
        )
    });
    album_jobs.chain(jobs).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::delete_guard::Origin;
    use crate::import_queue::test_support::row;
    use shared::download::DownloadState;
    use shared::failure::UserActionCode;

    #[test]
    fn unresolved_row_fails_with_file_not_found_after_download() {
        let failed = unresolved_row(row("a"), Path::new("/downloads"), Unresolved::NotOnDisk);

        assert_eq!(failed.state, DownloadState::Failed(UNRESOLVED.into()));
        assert_eq!(failed.error.as_deref(), Some(UNRESOLVED));
        let f = failed.failure.expect("a failure");
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::FileNotFoundAfterDownload,
                Stage::FileLookup,
                RetryAdvice::NotRetryable,
                UNRESOLVED
            )
        );
        let action = f.user_action.expect("a person fixes the download folder");
        assert_eq!(action.code, UserActionCode::FixDownloadPath);
        assert!(action.what.contains("/downloads"), "{}", action.what);
    }

    /// The file is there but went to another download: the row reads as
    /// before, and no folder setting is asked for.
    #[test]
    fn a_file_another_download_took_asks_for_no_folder_fix() {
        let failed = unresolved_row(row("a"), Path::new("/downloads"), Unresolved::Claimed);

        assert_eq!(failed.error.as_deref(), Some(UNRESOLVED));
        let f = failed.failure.expect("a failure");
        assert_eq!(
            (f.code, f.stage, f.retry),
            (
                FailureCode::FileNotFoundAfterDownload,
                Stage::FileLookup,
                RetryAdvice::NotRetryable
            )
        );
        assert_eq!(f.user_action, None);
    }

    #[test]
    fn import_job_takes_the_batchs_duplicates_rule() {
        let user = UserId::new("u");
        let job_for = |rule: &DuplicateRule| {
            import_job(
                Downloaded::File(PathBuf::from("/dl/X [12345678]/a.flac")),
                vec![row("a")],
                Path::new("/music"),
                &user,
                rule,
                Path::new("/dl"),
            )
        };
        assert!(matches!(
            job_for(&DuplicateRule::Skip).duplicates,
            Duplicates::Skip
        ));
        assert!(matches!(
            job_for(&DuplicateRule::Guarded).duplicates,
            Duplicates::Guarded { actor, origin: Origin::Ui } if actor == user
        ));
        let resolved = |destination: Option<&str>| ResolvedSlot {
            row: row("a"),
            path: PathBuf::from("/dl/X [12345678]/a.flac"),
            destination: destination.map(str::to_string),
        };
        let albums = album_jobs(
            vec![resolved(Some("X [12345678]")), resolved(None)],
            Path::new("/music"),
            &user,
            &DuplicateRule::Skip,
            Path::new("/dl"),
        );
        assert_eq!(albums.len(), 2);
        assert!(albums
            .iter()
            .all(|job| matches!(job.duplicates, Duplicates::Skip)));
    }

    #[test]
    fn an_album_imports_its_batch_folder_once_and_adopted_files_alone() {
        let batch = Some("X [12345678]".to_string());
        let resolved = vec![
            ResolvedSlot {
                row: row("a"),
                path: PathBuf::from("/dl/X [12345678]/a.flac"),
                destination: batch.clone(),
            },
            ResolvedSlot {
                row: row("adopted"),
                path: PathBuf::from("/dl/Other/f.flac"),
                destination: None,
            },
            ResolvedSlot {
                row: row("b"),
                path: PathBuf::from("/dl/X [12345678]/b_639097129778484198.flac"),
                destination: batch,
            },
        ];

        type Shown = (Vec<PathBuf>, Vec<PathBuf>, ImportMode, Vec<String>);
        let jobs: Vec<Shown> = album_jobs(
            resolved,
            Path::new("/music"),
            &UserId::new("u"),
            &DuplicateRule::Guarded,
            Path::new("/dl"),
        )
        .into_iter()
        .map(|job| {
            assert_eq!(job.cleanup_root.as_deref(), Some(Path::new("/dl")));
            assert_eq!(job.target, Path::new("/music"));
            let owner = job.owner.unwrap();
            assert_eq!(owner.user_id, UserId::new("u"));
            let items = owner.rows.into_iter().map(|r| r.item).collect();
            (job.sources, job.files, job.mode, items)
        })
        .collect();

        // An album's own files are its rows' files, not whatever else its folder holds.
        assert_eq!(
            jobs,
            vec![
                (
                    vec![PathBuf::from("/dl/X [12345678]")],
                    vec![
                        PathBuf::from("/dl/X [12345678]/a.flac"),
                        PathBuf::from("/dl/X [12345678]/b_639097129778484198.flac")
                    ],
                    ImportMode::Album,
                    vec!["a".to_string(), "b".to_string()]
                ),
                (
                    vec![PathBuf::from("/dl/Other/f.flac")],
                    vec![PathBuf::from("/dl/Other/f.flac")],
                    ImportMode::Singleton,
                    vec!["adopted".to_string()]
                ),
            ]
        );
    }

    /// A transfer the batch did not enqueue is looked for in the whole
    /// download folder; a file found there that went to another download is
    /// claimed, not missing.
    #[tokio::test(start_paused = true)]
    async fn an_adopted_file_another_download_took_is_claimed() {
        let base = tempfile::tempdir().unwrap();
        let album = base.path().join("Album");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("01.flac"), b"x").unwrap();
        let claimed = Mutex::new(HashSet::from([file_id(&album.join("01.flac")).unwrap()]));

        let found = resolve_on_disk(r"music\Album\01.flac", base.path(), None, &claimed).await;
        assert_eq!(found, Err(Unresolved::Claimed));
        let found = resolve_on_disk(r"music\Album\02.flac", base.path(), None, &claimed).await;
        assert_eq!(found, Err(Unresolved::NotOnDisk));
    }

    #[tokio::test]
    async fn resolve_on_disk_looks_in_the_batch_folder() {
        let base = tempfile::tempdir().unwrap();
        let folder = base.path().join("X [12345678]");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("01.flac"), b"x").unwrap();

        let found = resolve_on_disk(
            r"music\A\01.flac",
            base.path(),
            Some("X [12345678]"),
            &Mutex::new(HashSet::new()),
        )
        .await;
        assert_eq!(found, Ok(folder.join("01.flac")));
    }

    // Paused: the retries' backoff passes at once.
    #[tokio::test(start_paused = true)]
    async fn a_resolved_file_is_claimed_and_not_returned_twice() {
        let base = tempfile::tempdir().unwrap();
        let folder = base.path().join("X [12345678]");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("01.flac"), b"x").unwrap();
        std::fs::write(folder.join("01_639097129778484198.flac"), b"x").unwrap();
        let claimed = Mutex::new(HashSet::new());

        let first = resolve_on_disk("a/01.flac", base.path(), Some("X [12345678]"), &claimed);
        assert_eq!(first.await, Ok(folder.join("01.flac")));
        let second = resolve_on_disk("b/01.flac", base.path(), Some("X [12345678]"), &claimed);
        assert_eq!(second.await, Ok(folder.join("01_639097129778484198.flac")));
        assert_eq!(claimed.lock().unwrap().len(), 2);

        let third = resolve_on_disk("c/01.flac", base.path(), Some("X [12345678]"), &claimed);
        assert_eq!(third.await, Err(Unresolved::Claimed));
        let other = resolve_on_disk("c/02.flac", base.path(), Some("X [12345678]"), &claimed);
        assert_eq!(other.await, Err(Unresolved::NotOnDisk));
    }
}
