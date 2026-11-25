//! Track management core operations: inspect, move across folders/libraries, and re-tag/re-match.
//! Both the MCP tools and any future UI server functions call into this module.

pub mod audit;
pub mod extras;
pub mod inspect;
pub mod move_tracks;
pub mod retag;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage};
use soulbeet::navidrome::{NavidromeClient, ScanStatus};
use sqlx::SqlitePool;
use tracing::{debug, warn};

use crate::auth::UserId;
use crate::core::index::NavidromeOf;
use crate::core::scan::{self, ScanStart};
use crate::folder_policy::PolicyFolder;

/// Max distinct `song_ids` accepted in a single inspect / move / retag call.
pub const SONGS_PER_CALL: usize = 25;
/// Max tracks in a whole-album move / retag call.
pub const ALBUM_MAX_TRACKS: usize = 100;
/// How long to wait for an already-running Navidrome scan to become idle before touching files.
pub const IDLE_SCAN_WAIT_MAX: Duration = Duration::from_secs(10);
/// How long to wait for a triggered Navidrome scan to finish during a tool call.
pub const SCAN_WAIT_MAX: Duration = Duration::from_secs(20);
/// Poll interval while waiting for Navidrome's scanner.
const SCAN_POLL: Duration = Duration::from_millis(500);

/// Which tracks a move or retag call operates on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackSelection {
    Songs(Vec<String>),
    Album(String),
}

/// Outcome of triggering and waiting for a full-library Navidrome scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanWaitOutcome {
    /// Scan started and finished cleanly.
    Completed,
    /// No linked Navidrome account is an admin, so Soulbeet could not start a scan.
    NoScanAccount,
    /// Scan was started, or was already running, and did not finish within the wait budget.
    TimedOut,
    /// Scan failed or Navidrome could not be contacted.
    Failed(Failure),
}

/// Sidecar files associated with an audio file in its directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sidecars {
    /// Track-specific sidecars (e.g. `{stem}.lrc`, `{stem}.txt`).
    pub track_files: Vec<PathBuf>,
    /// Album-level artwork files (e.g. `cover.jpg`, `folder.png`).
    pub album_art: Vec<PathBuf>,
}

/// Whether `name` is a standard album cover image filename (e.g. `cover.jpg`, `folder.png`).
pub(crate) fn is_album_art(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let Some((stem, ext)) = lower.rsplit_once('.') else {
        return false;
    };
    matches!(ext, "jpg" | "jpeg" | "png" | "webp" | "gif")
        && matches!(stem, "cover" | "folder" | "front" | "album" | "artwork")
}

/// Finds track-specific sidecars (`{stem}.lrc`, `{stem}.txt`) and album artwork in `audio_file`'s
/// parent directory.
pub(crate) fn find_sidecars(audio_file: &Path) -> Sidecars {
    let mut found = Sidecars::default();
    let Some(dir) = audio_file.parent() else {
        return found;
    };
    let stem = audio_file
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == audio_file || !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if is_album_art(name) {
            found.album_art.push(path);
            continue;
        }
        if let Some(stem) = &stem {
            let lower = name.to_ascii_lowercase();
            if lower == format!("{stem}.lrc") || lower == format!("{stem}.txt") {
                found.track_files.push(path);
            }
        }
    }
    found.track_files.sort();
    found.album_art.sort();
    found
}

/// Whether `dir` still contains at least one audio file (direct children).
pub(crate) fn dir_has_audio(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        path.is_file() && soulbeet::audio::is_audio_path(&path)
    })
}

/// Moves `src` to `dst` atomically without altering file bytes or filename:
/// - refuses if `dst` already exists;
/// - tries `std::fs::rename` first (same filesystem);
/// - on cross-device error (`EXDEV`), writes to a hidden non-audio temp file
///   (`.soulbeet-move-<uuid>.tmp`, which Navidrome's watcher ignores), syncs it,
///   preserves `mtime`, renames into `dst`, and unlinks `src`.
pub(crate) fn atomic_move_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    if dst.try_exists()? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "Destination file already exists",
        ));
    }
    let Some(parent) = dst.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Destination path has no parent directory",
        ));
    };
    std::fs::create_dir_all(parent)?;
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if is_cross_device(&e) => cross_fs_move(src, dst, parent),
        Err(e) => Err(e),
    }
}

fn is_cross_device(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::CrossesDevices || e.raw_os_error() == Some(libc_exdev())
}

#[cfg(unix)]
fn libc_exdev() -> i32 {
    18 // EXDEV on Linux/macOS/Unix
}

#[cfg(not(unix))]
fn libc_exdev() -> i32 {
    -1
}

fn cross_fs_move(src: &Path, dst: &Path, parent: &Path) -> std::io::Result<()> {
    let src_meta = std::fs::metadata(src)?;
    let mtime = src_meta.modified().ok();
    let tmp = parent.join(format!(".soulbeet-move-{}.tmp", uuid::Uuid::new_v4()));
    let copy_result = (|| -> std::io::Result<()> {
        std::fs::copy(src, &tmp)?;
        let file = std::fs::OpenOptions::new().write(true).open(&tmp)?;
        if let Some(mtime) = mtime {
            let _ = file.set_modified(mtime);
        }
        file.sync_all()?;
        drop(file);
        if dst.try_exists()? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "Destination file appeared during move",
            ));
        }
        std::fs::rename(&tmp, dst)?;
        Ok(())
    })();
    if let Err(e) = copy_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::remove_file(src) {
        // Destination is already in place; try to roll back to avoid duplicate copies.
        let _ = std::fs::remove_file(dst);
        return Err(e);
    }
    Ok(())
}

/// Moves track sidecars (`{stem}.lrc`, `{stem}.txt`) to `dst_dir` with `new_stem` (or keeping their
/// own filename when `new_stem` is `None`), and copies or moves album artwork depending on whether
/// `src_dir` still holds other audio files. Non-fatal errors are returned as warnings.
pub(crate) fn transfer_sidecars(
    sidecars: &Sidecars,
    src_dir: &Path,
    dst_dir: &Path,
    new_stem: Option<&str>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for file in &sidecars.track_files {
        let Some(file_name) = file.file_name() else {
            continue;
        };
        let target_name = match (new_stem, file.extension().and_then(|e| e.to_str())) {
            (Some(stem), Some(ext)) => std::ffi::OsString::from(format!("{stem}.{ext}")),
            _ => file_name.to_os_string(),
        };
        let target = dst_dir.join(target_name);
        if target.exists() {
            continue;
        }
        if let Err(e) = atomic_move_file(file, &target) {
            warnings.push(format!(
                "Could not move sidecar {}: {e}",
                file.file_name().unwrap_or_default().to_string_lossy()
            ));
        }
    }
    let last_in_dir = !dir_has_audio(src_dir);
    for art in &sidecars.album_art {
        let Some(file_name) = art.file_name() else {
            continue;
        };
        let target = dst_dir.join(file_name);
        if target.exists() {
            if last_in_dir {
                let _ = std::fs::remove_file(art);
            }
            continue;
        }
        if last_in_dir {
            if let Err(e) = atomic_move_file(art, &target) {
                warnings.push(format!(
                    "Could not move cover art {}: {e}",
                    file_name.to_string_lossy()
                ));
            }
        } else if let Err(e) = std::fs::copy(art, &target) {
            warnings.push(format!(
                "Could not copy cover art {}: {e}",
                file_name.to_string_lossy()
            ));
        }
    }
    warnings
}

/// Waits up to `max_wait` for any running Navidrome scan to finish before moving or editing files,
/// so Navidrome does not scan the source and destination in separate passes.
pub(crate) async fn wait_for_idle_scan(
    navi: &NavidromeClient,
    max_wait: Duration,
) -> Option<ScanStatus> {
    let deadline = Instant::now() + max_wait;
    loop {
        match navi.get_scan_status().await {
            Ok(status) if !status.scanning => return Some(status),
            Ok(status) => {
                if Instant::now() >= deadline {
                    debug!("Navidrome scan still running after idle wait; proceeding");
                    return Some(status);
                }
                tokio::time::sleep(SCAN_POLL).await;
            }
            Err(e) => {
                debug!("Could not read Navidrome scan status before file operation: {e}");
                return None;
            }
        }
    }
}

/// Triggers a full-library quick scan via `core::scan::start_scan_for` (using the user's own
/// account or another linked Navidrome admin) and waits up to `max_wait` for it to finish.
pub(crate) async fn trigger_and_wait_scan(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    user: &UserId,
    own: &Arc<NavidromeClient>,
    stage: Stage,
    max_wait: Duration,
) -> ScanWaitOutcome {
    let baseline = match own.get_scan_status().await {
        Ok(status) => status.last_scan,
        Err(e) => {
            return ScanWaitOutcome::Failed(e.failure(stage, e.to_string()));
        }
    };
    match scan::start_scan_for(db, navidrome, &scan::SCAN_ROLES, user, own).await {
        Ok(ScanStart::Started) => {}
        Ok(ScanStart::NoAccount) => return ScanWaitOutcome::NoScanAccount,
        Err(e) => return ScanWaitOutcome::Failed(e.failure(stage, e.to_string())),
    }
    let deadline = Instant::now() + max_wait;
    loop {
        tokio::time::sleep(SCAN_POLL).await;
        match own.get_scan_status().await {
            Ok(status) if !status.scanning && status.ended_after(baseline) => {
                if !status.error.is_empty() {
                    return ScanWaitOutcome::Failed(
                        Failure::new(
                            FailureCode::ScanFailed,
                            Stage::Index,
                            RetryAdvice::SameSourceLater,
                            format!("Navidrome's scan failed: {}", status.error),
                        )
                        .with_cause(Service::Navidrome, None, status.error),
                    );
                }
                return ScanWaitOutcome::Completed;
            }
            Ok(_) => {
                if Instant::now() >= deadline {
                    return ScanWaitOutcome::TimedOut;
                }
            }
            Err(e) => {
                warn!("Could not poll Navidrome scan status: {e}");
                if Instant::now() >= deadline {
                    return ScanWaitOutcome::TimedOut;
                }
            }
        }
    }
}

/// Checks whether a folder path is inside a Discovery subfolder of any of the user's folders.
pub(crate) fn is_inside_discovery(candidate: &Path, user_folders: &[PolicyFolder]) -> bool {
    for f in user_folders {
        let Ok(root) = &f.path else { continue };
        let disc = root.join("Discovery");
        if candidate == disc || candidate.starts_with(&disc) {
            return true;
        }
    }
    false
}

/// Whether the user has a Navidrome account (own or another linked admin) capable of starting a
/// scan. Queries own `getUser` when unknown, then checks `scan_by`.
pub(crate) async fn can_trigger_scan(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    user: &UserId,
    own: &Arc<NavidromeClient>,
) -> Result<bool, Failure> {
    let admin = own
        .get_own_user()
        .await
        .map_err(|e| e.failure(Stage::Retag, e.to_string()))?
        .admin_role;
    scan::SCAN_ROLES.learn(user, own, admin, Instant::now());
    let by = scan::scan_by(db, navidrome, &scan::SCAN_ROLES, user, admin)
        .await
        .map_err(|e| e.failure(Stage::Retag, e.to_string()))?;
    // ScanBy::Own | ScanBy::Another => true; ScanBy::Unknown => another linked account exists
    // and might be an admin (we allow trying and verify during the intermediate scan);
    // ScanBy::Nobody => definitely no admin linked.
    Ok(by != scan::ScanBy::Nobody)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_track_sidecars_and_album_art() {
        let dir = tempfile::tempdir().unwrap();
        let track = dir.path().join("01 - Song.flac");
        std::fs::write(&track, b"audio").unwrap();
        std::fs::write(dir.path().join("01 - Song.lrc"), b"[00:01] hi").unwrap();
        std::fs::write(dir.path().join("02 - Other.lrc"), b"other").unwrap();
        std::fs::write(dir.path().join("cover.jpg"), b"jpg").unwrap();
        std::fs::write(dir.path().join("notes.pdf"), b"pdf").unwrap();

        let sidecars = find_sidecars(&track);
        assert_eq!(sidecars.track_files, vec![dir.path().join("01 - Song.lrc")]);
        assert_eq!(sidecars.album_art, vec![dir.path().join("cover.jpg")]);
    }

    #[test]
    fn atomic_move_refuses_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.flac");
        let dst = dir.path().join("b.flac");
        std::fs::write(&src, b"one").unwrap();
        std::fs::write(&dst, b"two").unwrap();
        let err = atomic_move_file(&src, &dst).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&src).unwrap(), b"one");
        assert_eq!(std::fs::read(&dst).unwrap(), b"two");
    }

    #[test]
    fn cross_fs_move_copies_preserves_bytes_and_removes_source() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src/01.flac");
        let dst = dir.path().join("dst/01.flac");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::fs::write(&src, b"exact bytes").unwrap();
        cross_fs_move(&src, &dst, dst.parent().unwrap()).unwrap();
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), b"exact bytes");
    }
}
