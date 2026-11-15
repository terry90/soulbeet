//! A file's identity, taken when it is checked and compared when it is removed.

use std::os::unix::fs::MetadataExt;

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
}
