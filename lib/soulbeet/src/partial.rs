//! Where a backend that writes files itself keeps them while they download, and how a finished
//! one lands in its batch folder.

use shared::download::{dotnet_ticks, landing_names, ticks_copy};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};

/// The folder under `DOWNLOAD_PATH` that holds every install's partial files.
pub const PARTIAL_FOLDER: &str = ".soulful-partial";

/// The lock files this process holds, by path: each one locked once, for the life of the process.
static HELD: LazyLock<Mutex<HashMap<PathBuf, File>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// One backend's partial files of one install: `<download_path>/.soulful-partial/<instance>/
/// <backend>/`. Two installs sharing a downloads folder have different instance ids, so neither
/// touches the other's files.
#[derive(Debug, Clone)]
pub struct PartialDir {
    dir: PathBuf,
}

impl PartialDir {
    /// The folder, made if missing. The first open of an instance in this process takes an
    /// exclusive lock on `<instance>/.lock`, held until the process ends; when another process
    /// holds it (a second server on the same database and downloads folder) the open fails.
    /// The lock covers one (database, downloads folder) pair: two servers on one database with
    /// different downloads folders do not see each other, and are not supported.
    pub fn open(download_path: &Path, instance: &str, backend: &str) -> io::Result<PartialDir> {
        for part in [instance, backend] {
            plain_name(part)?;
        }
        let root = download_path.join(PARTIAL_FOLDER).join(instance);
        std::fs::create_dir_all(&root)?;
        lock_once(&root.join(".lock"))?;
        let dir = root.join(backend);
        std::fs::create_dir_all(&dir)?;
        Ok(PartialDir { dir })
    }

    /// Where the partial file of the transfer `transfer_id` is, or the folder a backend's tool
    /// writes it in. A partial file is read through this path (its length, a checksum) and
    /// written only through `open_part`.
    pub fn part(&self, transfer_id: &str) -> PathBuf {
        self.dir.join(transfer_id)
    }

    /// The partial file of `transfer_id`, opened to write: made if missing, emptied unless
    /// `resume`, else written at its end. A part that is also a landed file (a landing cut
    /// between its link and its removal) is refused with `AlreadyExists`, so nothing rewrites a
    /// finished file: call `land` again, which then only removes the part.
    pub fn open_part(&self, transfer_id: &str, resume: bool) -> io::Result<File> {
        plain_name(transfer_id)?;
        let path = self.part(transfer_id);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .append(resume)
            .write(true)
            .open(&path)?;
        if file.metadata()?.nlink() > 1 {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already landed, call land to finish", path.display()),
            ));
        }
        if !resume {
            file.set_len(0)?;
        }
        Ok(file)
    }

    /// Deletes every partial file whose transfer id is not in `keep`.
    pub fn clear_except(&self, keep: &HashSet<String>) -> io::Result<()> {
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let kept = entry.file_name().to_str().is_some_and(|n| keep.contains(n));
            if kept {
                continue;
            }
            if entry.file_type()?.is_dir() {
                std::fs::remove_dir_all(entry.path())?;
            } else {
                std::fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }
}

/// Refuses a name that is empty, `.` or `..`, or holds a separator, so a join stays one folder
/// down.
fn plain_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("'{name}' is not a plain folder name"),
        ));
    }
    Ok(())
}

/// Locks `path` for this process, once: a later call for the same path finds it held here.
fn lock_once(path: &Path) -> io::Result<()> {
    let mut held = HELD.lock().unwrap_or_else(PoisonError::into_inner);
    if held.contains_key(path) {
        return Ok(());
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    // SAFETY: the descriptor belongs to `file`, which is open.
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if locked != 0 {
        let e = io::Error::last_os_error();
        return Err(match e.kind() {
            io::ErrorKind::WouldBlock => io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "Another Soulful server on this database and downloads folder holds {}",
                    path.display()
                ),
            ),
            _ => e,
        });
    }
    held.insert(path.to_path_buf(), file);
    Ok(())
}

/// Moves the finished partial file `part`, closed, to `<download_path>/<destination>/<name>`, `name`
/// being the first free name of `landing_names(remote_basename)`, else a `ticks_copy` of the
/// first one. It never replaces a file: it links `part` under a free name, then removes `part`.
/// When one of those names already is `part` (a crash between the link and the removal), it
/// only removes `part`. Returns where the file is, once the link and the removal are on disk,
/// so a backend may record the file landed when it returns.
pub fn land(
    part: &Path,
    download_path: &Path,
    destination: &str,
    remote_basename: &str,
) -> io::Result<PathBuf> {
    plain_name(destination)?;
    let names = landing_names(remote_basename);
    if remote_basename.is_empty() || names.iter().any(|n| n == "." || n == "..") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("'{remote_basename}' names no file"),
        ));
    }
    let folder = download_path.join(destination);
    if !folder.is_dir() {
        std::fs::create_dir_all(&folder)?;
        sync_dir(download_path)?;
    }
    let landed = match already_landed(part, &folder, &names)? {
        Some(landed) => landed,
        None => link_free(part, &folder, &names)?,
    };
    sync_dir(&folder)?;
    std::fs::remove_file(part)?;
    if let Some(parts) = part.parent() {
        sync_dir(parts)?;
    }
    Ok(landed)
}

/// Links `part` under the first free name of `names` in `folder`, else a ticks copy.
fn link_free(part: &Path, folder: &Path, names: &[String]) -> io::Result<PathBuf> {
    for name in names {
        let target = folder.join(name);
        match std::fs::hard_link(part, &target) {
            Ok(()) => return Ok(target),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    loop {
        let copy = folder.join(ticks_copy(
            &names[0],
            dotnet_ticks(std::time::SystemTime::now()),
        ));
        match std::fs::hard_link(part, &copy) {
            Ok(()) => return Ok(copy),
            // Another copy in the same tick: the next tick is free.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Puts the entries of the folder `dir` on disk: a name linked or removed there survives a
/// power cut once this returns. Some network and FUSE file systems cannot sync a folder and
/// answer EINVAL; PostgreSQL takes that answer as done too.
fn sync_dir(dir: &Path) -> io::Result<()> {
    match File::open(dir)?.sync_all() {
        Err(e)
            if matches!(
                e.raw_os_error(),
                Some(libc::EINVAL) | Some(libc::EOPNOTSUPP)
            ) =>
        {
            Ok(())
        }
        synced => synced,
    }
}

/// The name in `folder` that already is `part`'s file: one of `names` or a ticks copy of one.
fn already_landed(part: &Path, folder: &Path, names: &[String]) -> io::Result<Option<PathBuf>> {
    let meta = std::fs::metadata(part)?;
    let same = |path: &Path| {
        std::fs::symlink_metadata(path)
            .is_ok_and(|m| m.is_file() && m.dev() == meta.dev() && m.ino() == meta.ino())
    };
    if let Some(plain) = names.iter().map(|n| folder.join(n)).find(|p| same(p)) {
        return Ok(Some(plain));
    }
    for entry in std::fs::read_dir(folder)? {
        let path = entry?.path();
        let copy = path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
            names
                .iter()
                .any(|want| shared::download::is_ticks_copy(n, want))
        });
        if copy && same(&path) {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn instance() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// A finished partial file holding `bytes`, in a backend's partial folder under `root`.
    fn part(root: &Path, id: &str, bytes: &[u8]) -> PathBuf {
        let dir = PartialDir::open(root, &instance(), "archive").unwrap();
        dir.open_part(id, false).unwrap().write_all(bytes).unwrap();
        dir.part(id)
    }

    #[test]
    fn a_finished_file_lands_under_its_plain_name() {
        let root = tempfile::tempdir().unwrap();
        let p = part(root.path(), "t1", b"one");
        let landed = land(&p, root.path(), "Come Over [12345678]", "07 - Wake Up.flac").unwrap();
        assert_eq!(
            landed,
            root.path().join("Come Over [12345678]/07 - Wake Up.flac")
        );
        assert_eq!(std::fs::read(&landed).unwrap(), b"one");
        assert!(!p.exists());
    }

    #[test]
    fn a_taken_name_lands_as_a_ticks_copy_and_the_file_there_is_untouched() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("F");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("AC_ DC.flac"), b"first").unwrap();
        std::fs::write(folder.join("AC: DC.flac"), b"unix").unwrap();
        let p = part(root.path(), "t2", b"second");

        let landed = land(&p, root.path(), "F", "AC: DC.flac").unwrap();

        let name = landed.file_name().unwrap().to_str().unwrap();
        assert!(
            shared::download::is_ticks_copy(name, "AC_ DC.flac"),
            "{name}"
        );
        assert_eq!(std::fs::read(&landed).unwrap(), b"second");
        assert_eq!(std::fs::read(folder.join("AC_ DC.flac")).unwrap(), b"first");
        assert_eq!(std::fs::read(folder.join("AC: DC.flac")).unwrap(), b"unix");
        assert!(!p.exists());
    }

    #[test]
    fn a_name_free_only_in_its_unix_form_takes_that_form() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("F");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("AC_ DC.flac"), b"first").unwrap();
        let p = part(root.path(), "t3", b"second");
        assert_eq!(
            land(&p, root.path(), "F", "AC: DC.flac").unwrap(),
            folder.join("AC: DC.flac")
        );
    }

    #[test]
    fn a_destination_that_leaves_the_downloads_folder_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let p = part(root.path(), "t4", b"x");
        for destination in ["", ".", "..", "a/b", "a\\b", "/etc"] {
            let e = land(&p, root.path(), destination, "01.flac").unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{destination:?}");
        }
        for name in ["", "..", "."] {
            let e = land(&p, root.path(), "F", name).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{name:?}");
        }
        assert!(p.exists(), "a refused landing keeps the partial file");
    }

    #[test]
    fn a_landing_cut_between_link_and_removal_only_removes_the_part() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("F");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("01.flac"), b"someone else's").unwrap();
        let p = part(root.path(), "t5", b"mine");
        // The crash: linked as a copy, the part still there.
        let copy = folder.join(ticks_copy("01.flac", 639_255_968_000_000_000));
        std::fs::hard_link(&p, &copy).unwrap();

        assert_eq!(land(&p, root.path(), "F", "01.flac").unwrap(), copy);
        assert!(!p.exists());
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 2);
        assert_eq!(
            std::fs::read(folder.join("01.flac")).unwrap(),
            b"someone else's"
        );

        let p = part(root.path(), "t6", b"plain");
        let plain = folder.join("02.flac");
        std::fs::hard_link(&p, &plain).unwrap();
        assert_eq!(land(&p, root.path(), "F", "02.flac").unwrap(), plain);
        assert!(!p.exists());
    }

    #[test]
    fn a_part_is_written_from_the_start_or_resumed_at_its_end() {
        let root = tempfile::tempdir().unwrap();
        let dir = PartialDir::open(root.path(), &instance(), "archive").unwrap();
        dir.open_part("t1", false)
            .unwrap()
            .write_all(b"abc")
            .unwrap();
        dir.open_part("t1", true)
            .unwrap()
            .write_all(b"def")
            .unwrap();
        assert_eq!(std::fs::read(dir.part("t1")).unwrap(), b"abcdef");
        dir.open_part("t1", false).unwrap().write_all(b"x").unwrap();
        assert_eq!(std::fs::read(dir.part("t1")).unwrap(), b"x");
        for bad in ["", "..", "a/b"] {
            let e = dir.open_part(bad, false).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
        }
    }

    #[test]
    fn a_part_that_already_landed_is_never_written_again() {
        let root = tempfile::tempdir().unwrap();
        let dir = PartialDir::open(root.path(), &instance(), "archive").unwrap();
        dir.open_part("t1", false)
            .unwrap()
            .write_all(b"finished")
            .unwrap();
        // The crash: linked under its landing name, the part still there.
        let folder = root.path().join("F");
        std::fs::create_dir_all(&folder).unwrap();
        let landed = folder.join("01.flac");
        std::fs::hard_link(dir.part("t1"), &landed).unwrap();

        for resume in [false, true] {
            let e = dir.open_part("t1", resume).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{resume}");
        }
        assert_eq!(std::fs::read(&landed).unwrap(), b"finished");

        assert_eq!(
            land(&dir.part("t1"), root.path(), "F", "01.flac").unwrap(),
            landed
        );
        assert!(!dir.part("t1").exists());
        assert_eq!(std::fs::read(&landed).unwrap(), b"finished");
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 1);
    }

    #[test]
    fn a_lock_another_process_holds_refuses_the_open() {
        let root = tempfile::tempdir().unwrap();
        let id = instance();
        let lock = root.path().join(PARTIAL_FOLDER).join(&id).join(".lock");
        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        // flock locks of two open files conflict, as two processes' do.
        let other = File::create(&lock).unwrap();
        assert_eq!(
            unsafe { libc::flock(other.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let e = PartialDir::open(root.path(), &id, "archive").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
        assert!(e.to_string().contains("Another Soulful server"), "{e}");

        drop(other);
        PartialDir::open(root.path(), &id, "archive").unwrap();
        // Held by this process from now on: a second backend of the same install opens too.
        PartialDir::open(root.path(), &id, "gdstudio").unwrap();
    }

    #[test]
    fn two_instances_keep_their_own_folders() {
        let root = tempfile::tempdir().unwrap();
        let (a, b) = (instance(), instance());
        let mine = PartialDir::open(root.path(), &a, "archive").unwrap();
        let theirs = PartialDir::open(root.path(), &b, "archive").unwrap();
        mine.open_part("t1", false)
            .unwrap()
            .write_all(b"a")
            .unwrap();
        theirs
            .open_part("t1", false)
            .unwrap()
            .write_all(b"b")
            .unwrap();
        assert_ne!(mine.part("t1"), theirs.part("t1"));

        mine.clear_except(&HashSet::new()).unwrap();

        assert!(!mine.part("t1").exists());
        assert_eq!(std::fs::read(theirs.part("t1")).unwrap(), b"b");
        for bad in ["", "..", "a/b"] {
            assert!(
                PartialDir::open(root.path(), bad, "archive").is_err(),
                "{bad:?}"
            );
            assert!(PartialDir::open(root.path(), &a, bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn clearing_keeps_only_the_transfers_asked_for() {
        let root = tempfile::tempdir().unwrap();
        let dir = PartialDir::open(root.path(), &instance(), "ytdlp").unwrap();
        for id in ["keep", "drop"] {
            dir.open_part(id, false)
                .unwrap()
                .write_all(id.as_bytes())
                .unwrap();
        }
        std::fs::create_dir_all(dir.part("staging")).unwrap();
        std::fs::write(dir.part("staging").join("a.opus"), b"x").unwrap();

        dir.clear_except(&HashSet::from(["keep".to_string()]))
            .unwrap();

        assert!(dir.part("keep").exists());
        assert!(!dir.part("drop").exists());
        assert!(!dir.part("staging").exists());
    }
}
