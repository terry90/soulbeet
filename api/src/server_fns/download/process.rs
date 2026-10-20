use dioxus::logger::tracing::{info, warn};
use shared::download::{DownloadEvent, DownloadProgress, DownloadState};
use soulbeet::ImportMode;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::utils::{file_id, resolve_download_path, resolve_in_destination, FileId};
use crate::config::CONFIG;
use crate::globals::send_user_event;
use crate::import_queue::{ImportJob, JobOwner, IMPORT_QUEUE};

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

/// Where a monitor's finished downloads are and which of them it already
/// handed to the import queue.
#[derive(Clone)]
pub struct Handoff {
    pub target: PathBuf,
    pub username: String,
    pub download_base: PathBuf,
    /// Shared by every handoff of one monitor, so two slots of a batch with
    /// the same basename resolve to different files.
    pub claimed: Arc<Mutex<HashSet<FileId>>>,
}

/// Finds the finished downloads on disk and queues their imports. Returns
/// once they are queued: the queue reports each row's progress itself.
/// `rows` are already stamped with their batch and paired with the batch
/// folder they were enqueued into.
pub async fn submit_imports(rows: Vec<(DownloadProgress, Option<String>)>, handoff: Handoff) {
    let base = &handoff.download_base;
    let mut resolved = Vec::new();
    let mut unresolved = Vec::new();
    for (row, destination) in rows {
        match resolve_on_disk(&row.item, base, destination.as_deref(), &handoff.claimed).await {
            Some(path) => resolved.push((row, path, destination)),
            None => unresolved.push(DownloadProgress {
                state: DownloadState::Failed("Could not resolve file path".into()),
                error: Some("Could not resolve file path".into()),
                ..row
            }),
        }
    }
    if !unresolved.is_empty() {
        send_user_event(&handoff.username, DownloadEvent::Progress(unresolved)).await;
    }

    let jobs = plan_jobs(
        resolved,
        CONFIG.is_album_mode(),
        &handoff.target,
        &handoff.username,
        base,
    );
    info!(
        "Queueing {} import(s) into {:?}",
        jobs.len(),
        handoff.target
    );
    for job in jobs {
        IMPORT_QUEUE.submit(job).await;
    }
}

/// Album mode imports each batch folder as one album. A file found outside
/// any batch folder (a transfer the batch did not enqueue) is imported on its
/// own, since its folder may hold anything. Singleton mode imports each file
/// on its own.
fn plan_jobs(
    resolved: Vec<(DownloadProgress, PathBuf, Option<String>)>,
    album_mode: bool,
    target: &Path,
    username: &str,
    base: &Path,
) -> Vec<ImportJob> {
    let job = |source: PathBuf, mode: ImportMode, rows: Vec<DownloadProgress>| ImportJob {
        sources: vec![source],
        target: target.to_path_buf(),
        mode,
        owner: Some(JobOwner {
            username: username.to_string(),
            rows,
        }),
        snapshot_target: false,
        cleanup_root: Some(base.to_path_buf()),
    };

    let mut albums: Vec<(PathBuf, Vec<DownloadProgress>)> = Vec::new();
    let mut jobs = Vec::new();
    for (row, path, destination) in resolved {
        match destination.filter(|_| album_mode) {
            Some(destination) => {
                let folder = base.join(destination);
                match albums.iter_mut().find(|(f, _)| *f == folder) {
                    Some((_, rows)) => rows.push(row),
                    None => albums.push((folder, vec![row])),
                }
            }
            None => jobs.push(job(path, ImportMode::Singleton, vec![row])),
        }
    }
    let album_jobs = albums
        .into_iter()
        .map(|(folder, rows)| job(folder, ImportMode::Album, rows));
    album_jobs.chain(jobs).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import_queue::test_support::row;

    fn plan(album_mode: bool) -> Vec<(Vec<PathBuf>, ImportMode, Vec<String>)> {
        let batch = Some("X [12345678]".to_string());
        let resolved = vec![
            (
                row("a"),
                PathBuf::from("/dl/X [12345678]/a.flac"),
                batch.clone(),
            ),
            (row("foreign"), PathBuf::from("/dl/Other/f.flac"), None),
            (
                row("b"),
                PathBuf::from("/dl/X [12345678]/b_639097129778484198.flac"),
                batch,
            ),
        ];
        plan_jobs(
            resolved,
            album_mode,
            Path::new("/music"),
            "u",
            Path::new("/dl"),
        )
        .into_iter()
        .map(|job| {
            assert_eq!(job.cleanup_root.as_deref(), Some(Path::new("/dl")));
            let owner = job.owner.unwrap();
            assert_eq!(owner.username, "u");
            let items = owner.rows.into_iter().map(|r| r.item).collect();
            (job.sources, job.mode, items)
        })
        .collect()
    }

    #[test]
    fn album_mode_imports_the_batch_folder_once_and_foreign_files_alone() {
        assert_eq!(
            plan(true),
            vec![
                (
                    vec![PathBuf::from("/dl/X [12345678]")],
                    ImportMode::Album,
                    vec!["a".to_string(), "b".to_string()]
                ),
                (
                    vec![PathBuf::from("/dl/Other/f.flac")],
                    ImportMode::Singleton,
                    vec!["foreign".to_string()]
                ),
            ]
        );
    }

    #[test]
    fn singleton_mode_imports_each_file_alone() {
        let jobs = plan(false);
        assert_eq!(jobs.len(), 3);
        assert!(jobs
            .iter()
            .all(|(_, mode, items)| *mode == ImportMode::Singleton && items.len() == 1));
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
