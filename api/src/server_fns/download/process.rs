use dioxus::logger::tracing::{info, warn};
use shared::download::{DownloadProgress, DownloadState};
use soulbeet::ImportMode;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::utils::{file_id, resolve_download_path, resolve_in_destination, FileId};
use crate::import_queue::{ImportJob, JobOwner};

/// Maximum number of retries when waiting for a downloaded file to appear on disk.
/// With exponential backoff (500ms, 1s, 2s, 4s, 8s), this covers ~15.5s total.
const FILE_RESOLVE_MAX_RETRIES: u32 = 5;

/// Initial delay between file resolution retries.
const FILE_RESOLVE_INITIAL_DELAY_MS: u64 = 500;

/// Find a finished download on disk, retrying with exponential backoff:
/// slskd can report completion before the file is visible through a shared
/// volume mount. `destination` is the batch folder the file was enqueued
/// into; `None` (a transfer this batch did not enqueue) falls back to
/// searching the whole download folder. Files in `claimed` were already
/// handed off and are never returned; the file found is added to it. The
/// lock is held only while one attempt looks, never across a wait.
pub async fn resolve_on_disk(
    filename: &str,
    base: &Path,
    destination: Option<&str>,
    claimed: &Mutex<HashSet<FileId>>,
) -> Option<PathBuf> {
    let attempt = || {
        let mut claimed = claimed.lock().unwrap_or_else(|e| e.into_inner());
        let path = match destination {
            Some(destination) => resolve_in_destination(filename, base, destination, &claimed),
            None => resolve_download_path(filename, base).map(PathBuf::from),
        }?;
        let id = file_id(&path)?;
        claimed.insert(id).then_some(path)
    };
    if let Some(path) = attempt() {
        return Some(path);
    }

    let mut delay_ms = FILE_RESOLVE_INITIAL_DELAY_MS;
    for retry in 1..=FILE_RESOLVE_MAX_RETRIES {
        warn!(
            "File not found on disk (attempt {}), retrying in {}ms: {}",
            retry, delay_ms, filename
        );
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;

        if let Some(path) = attempt() {
            info!(
                "File appeared on disk after {}ms wait: {}",
                delay_ms, filename
            );
            return Some(path);
        }

        delay_ms *= 2;
    }

    None
}

/// Row text for a finished download whose file was not found on disk.
const UNRESOLVED: &str = "Could not resolve file path";

/// The Failed row of a finished download whose file was not found.
pub fn unresolved_row(row: DownloadProgress) -> DownloadProgress {
    DownloadProgress {
        state: DownloadState::Failed(UNRESOLVED.into()),
        error: Some(UNRESOLVED.into()),
        ..row
    }
}

/// A finished slot whose file was found on disk. `destination` is the batch
/// folder it was enqueued into, `None` for an adopted transfer.
pub struct ResolvedSlot {
    pub row: DownloadProgress,
    pub path: PathBuf,
    pub destination: Option<String>,
}

/// An import of `source` reporting to `username` through `rows`, which are
/// already stamped with their batch.
pub fn import_job(
    source: PathBuf,
    mode: ImportMode,
    rows: Vec<DownloadProgress>,
    target: &Path,
    username: &str,
    base: &Path,
) -> ImportJob {
    ImportJob {
        sources: vec![source],
        target: target.to_path_buf(),
        mode,
        owner: Some(JobOwner {
            username: username.to_string(),
            rows,
        }),
        snapshot_target: false,
        cleanup_root: Some(base.to_path_buf()),
    }
}

/// The imports of an album batch: its folder as one album, and each file
/// found outside it (an adopted transfer) on its own, since that file's
/// folder may hold anything.
pub fn album_jobs(
    resolved: Vec<ResolvedSlot>,
    target: &Path,
    username: &str,
    base: &Path,
) -> Vec<ImportJob> {
    let mut albums: Vec<(PathBuf, Vec<DownloadProgress>)> = Vec::new();
    let mut jobs = Vec::new();
    for slot in resolved {
        match slot.destination {
            Some(destination) => {
                let folder = base.join(destination);
                match albums.iter_mut().find(|(f, _)| *f == folder) {
                    Some((_, rows)) => rows.push(slot.row),
                    None => albums.push((folder, vec![slot.row])),
                }
            }
            None => jobs.push(import_job(
                slot.path,
                ImportMode::Singleton,
                vec![slot.row],
                target,
                username,
                base,
            )),
        }
    }
    let album_jobs = albums
        .into_iter()
        .map(|(folder, rows)| import_job(folder, ImportMode::Album, rows, target, username, base));
    album_jobs.chain(jobs).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import_queue::test_support::row;

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

        let jobs: Vec<(Vec<PathBuf>, ImportMode, Vec<String>)> =
            album_jobs(resolved, Path::new("/music"), "u", Path::new("/dl"))
                .into_iter()
                .map(|job| {
                    assert_eq!(job.cleanup_root.as_deref(), Some(Path::new("/dl")));
                    assert_eq!(job.target, Path::new("/music"));
                    let owner = job.owner.unwrap();
                    assert_eq!(owner.username, "u");
                    let items = owner.rows.into_iter().map(|r| r.item).collect();
                    (job.sources, job.mode, items)
                })
                .collect();

        assert_eq!(
            jobs,
            vec![
                (
                    vec![PathBuf::from("/dl/X [12345678]")],
                    ImportMode::Album,
                    vec!["a".to_string(), "b".to_string()]
                ),
                (
                    vec![PathBuf::from("/dl/Other/f.flac")],
                    ImportMode::Singleton,
                    vec!["adopted".to_string()]
                ),
            ]
        );
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
        assert_eq!(found, Some(folder.join("01.flac")));
    }

    #[tokio::test]
    async fn a_resolved_file_is_claimed_and_not_returned_twice() {
        let base = tempfile::tempdir().unwrap();
        let folder = base.path().join("X [12345678]");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("01.flac"), b"x").unwrap();
        std::fs::write(folder.join("01_639097129778484198.flac"), b"x").unwrap();
        let claimed = Mutex::new(HashSet::new());

        let first = resolve_on_disk("a/01.flac", base.path(), Some("X [12345678]"), &claimed);
        assert_eq!(first.await, Some(folder.join("01.flac")));
        let second = resolve_on_disk("b/01.flac", base.path(), Some("X [12345678]"), &claimed);
        assert_eq!(
            second.await,
            Some(folder.join("01_639097129778484198.flac"))
        );
        assert_eq!(claimed.lock().unwrap().len(), 2);
    }
}
