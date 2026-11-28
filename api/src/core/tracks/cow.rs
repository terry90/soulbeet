//! Copy-on-Write (CoW) hardlink breaker for in-place audio file mutations.
//!
//! When a track is shared via filesystem hardlink (`st_nlink > 1`), any
//! in-place tag write (such as `beet modify` or `beet import -C -M`) would
//! silently mutate the other user's file bytes, size, and mtime. Breaking the
//! hardlink into a private inode before writing guarantees complete isolation.

use std::path::Path;

/// If `path` is a hardlinked regular file (`st_nlink > 1`), atomically replaces
/// the directory entry with a private copy (`st_nlink == 1`), preserving its
/// content and modification time. Returns `Ok(true)` if a hardlink was broken,
/// `Ok(false)` if the file already had a single link.
pub fn break_hardlink_if_shared(path: &Path) -> std::io::Result<bool> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Refusing to break hardlink on a symbolic link",
        ));
    }
    if !meta.is_file() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() <= 1 {
            return Ok(false);
        }
    }
    #[cfg(not(unix))]
    {
        return Ok(false);
    }

    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "File path has no parent directory",
        )
    })?;
    let mtime = meta.modified().ok();
    let tmp = parent.join(format!(".soulbeet-cow-{}.tmp", uuid::Uuid::new_v4()));

    let result = (|| -> std::io::Result<()> {
        let mut src = std::fs::File::open(path)?;
        let mut dst = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        std::io::copy(&mut src, &mut dst)?;
        let _ = dst.set_permissions(meta.permissions());
        if let Some(mtime) = mtime {
            let _ = dst.set_modified(mtime);
        }
        dst.sync_all()?;
        drop(dst);
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();

    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn breaks_shared_hardlink_and_isolates_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("alice.flac");
        let b = dir.path().join("bob.flac");
        std::fs::write(&a, b"original audio bytes").unwrap();
        std::fs::hard_link(&a, &b).unwrap();
        assert_eq!(std::fs::symlink_metadata(&a).unwrap().nlink(), 2);
        assert_eq!(
            std::fs::symlink_metadata(&a).unwrap().ino(),
            std::fs::symlink_metadata(&b).unwrap().ino()
        );

        let mtime_before = std::fs::symlink_metadata(&a).unwrap().modified().unwrap();
        assert!(break_hardlink_if_shared(&a).unwrap());

        assert_eq!(std::fs::symlink_metadata(&a).unwrap().nlink(), 1);
        assert_eq!(std::fs::symlink_metadata(&b).unwrap().nlink(), 1);
        assert_ne!(
            std::fs::symlink_metadata(&a).unwrap().ino(),
            std::fs::symlink_metadata(&b).unwrap().ino()
        );
        assert_eq!(
            std::fs::symlink_metadata(&a).unwrap().modified().unwrap(),
            mtime_before
        );

        // Mutating A in place leaves B untouched.
        std::fs::write(&a, b"retagged bytes for alice").unwrap();
        assert_eq!(std::fs::read(&b).unwrap(), b"original audio bytes");

        // Second call on private file is a no-op.
        assert!(!break_hardlink_if_shared(&a).unwrap());
    }
}
