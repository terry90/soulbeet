//! A file's identity, taken when it is checked and compared when it is removed.

use rustix::fs::{AtFlags, Mode, OFlags};
use rustix::io::Errno;
use std::ffi::OsStr;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

/// What makes a file the one that was checked: the same inode, size and modification time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FileId {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
}

impl FileId {
    /// The identity in `meta`, as `std::fs::symlink_metadata` read it.
    pub(super) fn of(meta: &std::fs::Metadata) -> FileId {
        FileId {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
        }
    }

    /// The identity in `stat`, cast as std casts the same fields (std::os::linux::fs::MetadataExt).
    #[allow(
        clippy::unnecessary_cast,
        reason = "the widths of struct stat's fields differ between targets"
    )]
    fn of_stat(stat: &rustix::fs::Stat) -> FileId {
        FileId {
            dev: stat.st_dev as u64,
            ino: stat.st_ino as u64,
            size: stat.st_size as u64,
            mtime: stat.st_mtime as i64,
            mtime_nsec: stat.st_mtime_nsec as i64,
        }
    }
}

/// Why a file was not removed.
pub(super) enum UnlinkError {
    /// The path no longer leads, without a symbolic link, to the file that was checked.
    Changed,
    Io(std::io::Error),
}

impl From<Errno> for UnlinkError {
    fn from(e: Errno) -> UnlinkError {
        match e {
            Errno::LOOP | Errno::NOTDIR | Errno::NOENT => UnlinkError::Changed,
            e => UnlinkError::Io(e.into()),
        }
    }
}

/// Opens every directory of `path` from "/" with O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC, stats the last
/// component with AT_SYMLINK_NOFOLLOW, and unlinks it only when its identity is `expected`.
/// A symlinked component (ELOOP, ENOTDIR), a missing one (ENOENT) or another identity is Changed.
pub(super) fn unlink_if_same(path: &Path, expected: FileId) -> Result<(), UnlinkError> {
    let (dir, name, found) = located(path)?;
    if found != expected {
        return Err(UnlinkError::Changed);
    }
    rustix::fs::unlinkat(&dir, name, AtFlags::empty())?;
    Ok(())
}

/// What a look at the path of a file that was checked finds.
#[derive(Debug)]
pub(super) enum Look {
    /// The checked file (its device and inode), whether it changed since or not.
    There,
    /// Nothing is at the path.
    Gone,
    /// Another file (another device or inode) is at the path: the checked file is gone.
    Replaced,
    /// The path cannot be read without following a link (ELOOP, ENOTDIR), or reading it failed.
    Unreadable(std::io::Error),
}

/// Looks at `path`, read as unlink_if_same reads it, for the file whose identity was `expected`.
pub(super) fn look(path: &Path, expected: FileId) -> Look {
    match located(path) {
        Ok((_, _, found)) if (found.dev, found.ino) == (expected.dev, expected.ino) => Look::There,
        Ok(_) => Look::Replaced,
        Err(Located::Errno(Errno::NOENT)) => Look::Gone,
        Err(e) => Look::Unreadable(e.into()),
    }
}

/// Why located found nothing.
enum Located {
    Errno(Errno),
    /// The path is not canonical.
    NotCanonical(std::io::Error),
}

impl From<Errno> for Located {
    fn from(e: Errno) -> Located {
        Located::Errno(e)
    }
}

impl From<Located> for std::io::Error {
    fn from(e: Located) -> std::io::Error {
        match e {
            Located::Errno(e) => e.into(),
            Located::NotCanonical(e) => e,
        }
    }
}

impl From<Located> for UnlinkError {
    fn from(e: Located) -> UnlinkError {
        match e {
            Located::Errno(e) => e.into(),
            Located::NotCanonical(e) => UnlinkError::Io(e),
        }
    }
}

/// The directory of the canonical `path`, opened from "/" without following a link, the name
/// of `path` in it, and the identity of what that name holds.
fn located(path: &Path) -> Result<(OwnedFd, &OsStr, FileId), Located> {
    let canonical = path.is_absolute()
        && path
            .components()
            .skip(1)
            .all(|c| matches!(c, Component::Normal(_)));
    let (true, Some(parent), Some(name)) = (canonical, path.parent(), path.file_name()) else {
        return Err(Located::NotCanonical(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not a canonical path to a file", path.display()),
        )));
    };
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut dir: OwnedFd = rustix::fs::open("/", flags, Mode::empty())?;
    for child in parent.iter().skip(1) {
        dir = rustix::fs::openat(&dir, child, flags, Mode::empty())?;
    }
    let stat = rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
    Ok((dir, name, FileId::of_stat(&stat)))
}
