//! Filesystem linking primitives for Track Sharing and Legacy Folder Migration.
//!
//! Strategy:
//! 1. Try `std::fs::hard_link` first (0 extra disk bytes on the same filesystem).
//! 2. On cross-device (`EXDEV`), try Linux `FICLONE` reflink (0 extra disk bytes
//!    on CoW filesystems like Btrfs/XFS).
//! 3. If reflink is unsupported, perform an atomic regular file copy preserving
//!    modification time and syncing to disk.

use super::{is_album_art, Sidecars};
use crate::models::track_share::LinkMode;
use std::ffi::OsStr;
use std::path::Path;

/// Predicts whether `src` and `dst_root` sit on the same filesystem device
/// (`st_dev` match -> `LinkMode::Hardlink`, otherwise `LinkMode::Copy` as a
/// conservative dry-run estimate). Walks `dst_root.ancestors()` to find the
/// nearest existing directory so dry-run previews stay accurate even before
/// `dst_root` is created on disk.
pub fn predict_link_mode(src: &Path, dst_root: &Path) -> LinkMode {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(a) = std::fs::symlink_metadata(src) else {
            return LinkMode::Copy;
        };
        for ancestor in dst_root.ancestors() {
            if let Ok(b) = std::fs::metadata(ancestor) {
                return if a.dev() == b.dev() {
                    LinkMode::Hardlink
                } else {
                    LinkMode::Copy
                };
            }
        }
        LinkMode::Copy
    }
    #[cfg(not(unix))]
    {
        let _ = (src, dst_root);
        LinkMode::Copy
    }
}

/// Hardlinks, reflinks, or atomically copies `src` to `dst` without altering
/// `src`. Refuses symbolic links on `src` and refuses existing destinations.
pub fn link_or_copy_file(src: &Path, dst: &Path) -> std::io::Result<LinkMode> {
    let src_meta = std::fs::symlink_metadata(src)?;
    if src_meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Refusing to share a symbolic link",
        ));
    }
    if !src_meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Source path is not a regular file",
        ));
    }
    if dst.try_exists()? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "Destination file already exists",
        ));
    }
    let parent = dst.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Destination path has no parent directory",
        )
    })?;
    std::fs::create_dir_all(parent)?;

    match std::fs::hard_link(src, dst) {
        Ok(()) => return Ok(LinkMode::Hardlink),
        Err(e) if is_cross_device(&e) => {}
        Err(e) => return Err(e),
    }

    cross_fs_link_or_copy(src, dst, parent, &src_meta)
}

fn is_cross_device(e: &std::io::Error) -> bool {
    #[cfg(unix)]
    const EXDEV: i32 = 18;
    #[cfg(not(unix))]
    const EXDEV: i32 = -1;
    e.kind() == std::io::ErrorKind::CrossesDevices || e.raw_os_error() == Some(EXDEV)
}

fn cross_fs_link_or_copy(
    src: &Path,
    dst: &Path,
    parent: &Path,
    src_meta: &std::fs::Metadata,
) -> std::io::Result<LinkMode> {
    let mtime = src_meta.modified().ok();
    let tmp = parent.join(format!(".soulbeet-share-{}.tmp", uuid::Uuid::new_v4()));

    let result = (|| -> std::io::Result<LinkMode> {
        let mut src_file = std::fs::File::open(src)?;
        let mut dst_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;

        #[cfg(target_os = "linux")]
        let mode = match rustix::fs::ioctl_ficlone(&dst_file, &src_file) {
            Ok(()) => LinkMode::Reflink,
            Err(_) => {
                std::io::copy(&mut src_file, &mut dst_file)?;
                LinkMode::Copy
            }
        };
        #[cfg(not(target_os = "linux"))]
        let mode = {
            std::io::copy(&mut src_file, &mut dst_file)?;
            LinkMode::Copy
        };

        let _ = dst_file.set_permissions(src_meta.permissions());
        if let Some(mtime) = mtime {
            let _ = dst_file.set_modified(mtime);
        }
        dst_file.sync_all()?;
        drop(dst_file);

        if dst.try_exists()? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "Destination file appeared during share",
            ));
        }
        std::fs::rename(&tmp, dst)?;
        Ok(mode)
    })();

    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    result
}

/// Non-destructively shares track sidecars (`{stem}.lrc`, `{stem}.txt`) and
/// album artwork (`cover.jpg`, etc.) into `dst_dir`. Never removes source files
/// and never overwrites existing files or existing album art in `dst_dir`.
pub fn share_sidecars(sidecars: &Sidecars, dst_dir: &Path, new_stem: Option<&OsStr>) -> Vec<String> {
    let mut warnings = Vec::new();
    for file in &sidecars.track_files {
        let Some(file_name) = file.file_name() else {
            continue;
        };
        let target_name = match (new_stem.and_then(|s| s.to_str()), file.extension().and_then(|e| e.to_str())) {
            (Some(stem), Some(ext)) => std::ffi::OsString::from(format!("{stem}.{ext}")),
            _ => file_name.to_os_string(),
        };
        let target = dst_dir.join(target_name);
        if target.exists() {
            continue;
        }
        if let Err(e) = link_or_copy_file(file, &target) {
            warnings.push(format!(
                "Could not share sidecar {}: {e}",
                file_name.to_string_lossy()
            ));
        }
    }

    let dst_already_has_art = std::fs::read_dir(dst_dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            entry.path().is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(is_album_art)
        });

    if !dst_already_has_art {
        for art in &sidecars.album_art {
            let Some(file_name) = art.file_name() else {
                continue;
            };
            let target = dst_dir.join(file_name);
            if target.exists() {
                continue;
            }
            if let Err(e) = link_or_copy_file(art, &target) {
                warnings.push(format!(
                    "Could not share cover art {}: {e}",
                    file_name.to_string_lossy()
                ));
            }
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, MetadataExt};

    #[test]
    fn hardlinks_same_filesystem_and_refuses_existing_or_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("alice/Artist/Album/01.flac");
        let dst = dir.path().join("bob/Artist/Album/01.flac");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, b"flac audio").unwrap();

        let mode = link_or_copy_file(&src, &dst).unwrap();
        assert_eq!(mode, LinkMode::Hardlink);
        assert_eq!(std::fs::symlink_metadata(&src).unwrap().nlink(), 2);
        assert_eq!(
            std::fs::symlink_metadata(&src).unwrap().ino(),
            std::fs::symlink_metadata(&dst).unwrap().ino()
        );

        // Existing destination refused cleanly.
        let err = link_or_copy_file(&src, &dst).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);

        // Symlink source refused cleanly.
        let link_path = dir.path().join("alice/sym.flac");
        symlink(&src, &link_path).unwrap();
        let err = link_or_copy_file(&link_path, &dir.path().join("bob/sym.flac")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn share_sidecars_preserves_source_and_does_not_overwrite_existing_art() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("alice/Album");
        let dst_dir = dir.path().join("bob/Album");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&dst_dir).unwrap();

        let lrc = src_dir.join("01.lrc");
        let art = src_dir.join("cover.jpg");
        std::fs::write(&lrc, b"[00:01] hello").unwrap();
        std::fs::write(&art, b"alice cover").unwrap();
        std::fs::write(dst_dir.join("folder.png"), b"bob cover").unwrap();

        let sidecars = Sidecars {
            track_files: vec![lrc.clone()],
            album_art: vec![art.clone()],
        };
        let warnings = share_sidecars(&sidecars, &dst_dir, None);
        assert!(warnings.is_empty());
        assert!(dst_dir.join("01.lrc").exists());
        assert!(lrc.exists());
        assert!(!dst_dir.join("cover.jpg").exists(), "should keep bob's existing art");
    }

    #[test]
    fn predict_link_mode_walks_ancestors_when_dst_root_does_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("alice/01.flac");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, b"audio").unwrap();

        let missing_dst_root = dir.path().join("bob/not/created/yet");
        assert!(!missing_dst_root.exists());
        assert_eq!(predict_link_mode(&src, &missing_dst_root), LinkMode::Hardlink);
    }
}
