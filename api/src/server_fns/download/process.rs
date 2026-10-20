use dioxus::logger::tracing::{info, warn};
use shared::download::{DownloadEvent, DownloadProgress, DownloadState};
use soulbeet::ImportMode;
use std::path::{Path, PathBuf};

use super::utils::resolve_download_path;
use crate::config::CONFIG;
use crate::globals::send_user_event;
use crate::import_queue::{ImportJob, JobOwner, IMPORT_QUEUE};

/// Maximum number of retries when waiting for a downloaded file to appear on disk.
/// With exponential backoff (500ms, 1s, 2s, 4s, 8s), this covers ~15.5s total.
const FILE_RESOLVE_MAX_RETRIES: u32 = 5;

/// Initial delay between file resolution retries.
const FILE_RESOLVE_INITIAL_DELAY_MS: u64 = 500;

/// Resolve a download path with retries. When a download completes very fast
/// (e.g. file already cached in slskd), the API may report completion before
/// the file is visible on disk via shared volume mounts. This retries with
/// exponential backoff to handle that lag.
async fn resolve_download_path_with_retry(filename: &str, download_base: &Path) -> Option<String> {
    if let Some(path) = resolve_download_path(filename, download_base) {
        return Some(path);
    }

    let mut delay_ms = FILE_RESOLVE_INITIAL_DELAY_MS;
    for attempt in 1..=FILE_RESOLVE_MAX_RETRIES {
        warn!(
            "File not found on disk (attempt {}), retrying in {}ms: {}",
            attempt, delay_ms, filename
        );
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;

        if let Some(path) = resolve_download_path(filename, download_base) {
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

/// Finds the finished downloads on disk and queues their imports. Returns
/// once they are queued: the queue reports each row's progress itself.
/// `rows` are already stamped with their batch.
pub async fn submit_imports(rows: Vec<DownloadProgress>, target: PathBuf, username: String) {
    let base = CONFIG.download_path().clone();
    let mut resolved = Vec::new();
    let mut unresolved = Vec::new();
    for row in rows {
        match resolve_download_path_with_retry(&row.item, &base).await {
            Some(path) => resolved.push((row, PathBuf::from(path))),
            None => unresolved.push(DownloadProgress {
                state: DownloadState::Failed("Could not resolve file path".into()),
                error: Some("Could not resolve file path".into()),
                ..row
            }),
        }
    }
    if !unresolved.is_empty() {
        send_user_event(&username, DownloadEvent::Progress(unresolved)).await;
    }

    let jobs = plan_jobs(resolved, CONFIG.is_album_mode(), &target, &username, &base);
    info!("Queueing {} import(s) into {:?}", jobs.len(), target);
    for job in jobs {
        IMPORT_QUEUE.submit(job).await;
    }
}

/// Album mode imports each folder as one album, except files lying directly
/// in the download folder. Singleton mode imports each file on its own.
fn plan_jobs(
    resolved: Vec<(DownloadProgress, PathBuf)>,
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
    for (row, path) in resolved {
        match path.parent().filter(|dir| album_mode && *dir != base) {
            Some(dir) => match albums.iter_mut().find(|(d, _)| d == dir) {
                Some((_, rows)) => rows.push(row),
                None => albums.push((dir.to_path_buf(), vec![row])),
            },
            None => jobs.push(job(path, ImportMode::Singleton, vec![row])),
        }
    }
    let album_jobs = albums
        .into_iter()
        .map(|(dir, rows)| job(dir, ImportMode::Album, rows));
    album_jobs.chain(jobs).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import_queue::test_support::row;

    fn plan(album_mode: bool) -> Vec<(Vec<PathBuf>, ImportMode, Vec<String>)> {
        let resolved = vec![
            (row("a"), PathBuf::from("/dl/X/a.flac")),
            (row("loose"), PathBuf::from("/dl/loose.flac")),
            (row("b"), PathBuf::from("/dl/X/b.flac")),
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
    fn album_mode_imports_each_folder_once_and_loose_files_alone() {
        assert_eq!(
            plan(true),
            vec![
                (
                    vec![PathBuf::from("/dl/X")],
                    ImportMode::Album,
                    vec!["a".to_string(), "b".to_string()]
                ),
                (
                    vec![PathBuf::from("/dl/loose.flac")],
                    ImportMode::Singleton,
                    vec!["loose".to_string()]
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
}
