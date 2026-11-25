#[cfg(feature = "server")]
use shared::download::{is_ticks_copy, landing_names, sanitize_filename};
#[cfg(feature = "server")]
use std::collections::HashSet;
#[cfg(feature = "server")]
use std::path::{Path, PathBuf};
#[cfg(feature = "server")]
use tracing::warn;

/// Find the file of a finished transfer the batch adopted: one it did not
/// enqueue, so slskd saved it with no batch folder of the batch's. That is
/// where slskd's default Subdirectory pattern `${SOURCE_DIRECTORY}` puts it,
/// `<download_base>/<remote parent directory>/<file>` (the file alone when the
/// remote name has no directory), the parent sanitized as slskd sanitizes a
/// folder name, and is looked for there as [`resolve_in_destination`] looks in
/// a batch folder. Nothing else in the download folder is searched: it holds
/// every batch's and every user's downloads.
///
/// A remote name is the peer's text. One that is absolute, starts with a
/// drive, or steps through `.` or `..` names nothing here, and only a regular
/// file strictly inside the download folder, symlinks resolved, is returned.
#[cfg(feature = "server")]
pub fn resolve_adopted(
    filename: &str,
    download_base: &Path,
    exclude: &HashSet<FileId>,
) -> Result<PathBuf, Unresolved> {
    let normalized = filename.replace('\\', "/");
    let components: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
    if !is_plain_remote_name(&normalized, &components) {
        warn!("Refusing to look for the remote file '{filename}': its name leaves its own folder");
        return Err(Unresolved::NotOnDisk);
    }
    let folder = match components.as_slice() {
        [.., parent, _] => sanitize_filename(parent),
        _ => String::new(),
    };
    let path = resolve_in_destination(&normalized, download_base, &folder, exclude)?;
    if !is_inside(&path, download_base) {
        warn!(
            "Refusing {}: it is not a file inside the download folder {}",
            path.display(),
            download_base.display()
        );
        return Err(Unresolved::NotOnDisk);
    }
    Ok(path)
}

/// Whether a remote name, with `/` separators, is a relative path of plain
/// names: not absolute, no drive, no `.` or `..` step.
#[cfg(feature = "server")]
fn is_plain_remote_name(normalized: &str, components: &[&str]) -> bool {
    let drive = |c: &str| c.len() == 2 && c.ends_with(':') && c.as_bytes()[0].is_ascii_alphabetic();
    !components.is_empty()
        && !normalized.starts_with('/')
        && !components.first().is_some_and(|c| drive(c))
        && !components.iter().any(|c| *c == "." || *c == "..")
}

/// Whether `path` is a regular file (not a link) whose real path is strictly
/// inside the real path of `base`.
#[cfg(feature = "server")]
fn is_inside(path: &Path, base: &Path) -> bool {
    let regular = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file());
    let real = (std::fs::canonicalize(path), std::fs::canonicalize(base));
    regular && matches!(real, (Ok(p), Ok(b)) if p != b && p.starts_with(&b))
}

/// A file's identity on disk, independent of its name: beets can move a
/// handed-off file away and slskd can then write the next download under the
/// same name, which must not count as the file already handed off. Birth
/// time, where the file system records it, tells a reused inode number apart.
#[cfg(feature = "server")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId {
    dev: u64,
    ino: u64,
    born: Option<std::time::SystemTime>,
}

#[cfg(feature = "server")]
pub fn file_id(path: &Path) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    Some(FileId {
        dev: meta.dev(),
        ino: meta.ino(),
        born: meta.created().ok(),
    })
}

/// Why no file was found for a finished download.
#[cfg(feature = "server")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unresolved {
    /// No file by its name is where Soulful looks.
    NotOnDisk,
    /// Files by its name are there, but each was already handed off for
    /// another download.
    Claimed,
}

/// Find a finished download in its batch folder `base/destination`.
///
/// A backend keeps the remote basename under one of its `landing_names` and,
/// when that name is already taken, writes a `ticks_copy` of it. Two files of
/// one batch with the same basename therefore land as the plain name and its
/// copies. The plain name is tried first, then the copies oldest first, and
/// files in `exclude` (already handed off) are skipped.
#[cfg(feature = "server")]
pub fn resolve_in_destination(
    filename: &str,
    base: &Path,
    destination: &str,
    exclude: &HashSet<FileId>,
) -> Result<PathBuf, Unresolved> {
    let normalized = filename.replace('\\', "/");
    let remote_name = normalized
        .rsplit('/')
        .find(|s| !s.is_empty())
        .ok_or(Unresolved::NotOnDisk)?;
    let folder = base.join(destination);
    let names = landing_names(remote_name);

    let mut excluded = false;
    let mut usable = |path: &PathBuf| match path.is_file().then(|| file_id(path)).flatten() {
        Some(id) if exclude.contains(&id) => {
            excluded = true;
            false
        }
        id => id.is_some(),
    };
    if let Some(plain) = names.iter().map(|n| folder.join(n)).find(&mut usable) {
        return Ok(plain);
    }

    let mut copies: Vec<PathBuf> = std::fs::read_dir(&folder)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| names.iter().any(|want| is_ticks_copy(n, want)))
        })
        .collect();
    // Equal-width ticks, so name order is age order.
    copies.sort();
    let copy = copies.into_iter().find(&mut usable);
    match (copy, excluded) {
        (Some(copy), _) => Ok(copy),
        (None, true) => Err(Unresolved::Claimed),
        (None, false) => Err(Unresolved::NotOnDisk),
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;

    fn temp_base(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "soulful-utils-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn resolves_windows_sanitized_basename() {
        let base = temp_base("win");
        let album = base.join("Album");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("AC_ DC - 01.mp3"), b"x").unwrap();

        let resolved =
            resolve_adopted(r"music\Album\AC: DC - 01.mp3", &base, &HashSet::new()).unwrap();
        assert!(resolved.ends_with("AC_ DC - 01.mp3"));

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn resolves_unix_sanitized_basename() {
        // A Linux slskd only replaces '/' and NUL, so ':' survives on disk.
        let base = temp_base("unix");
        let album = base.join("Album");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("AC: DC - 01.mp3"), b"x").unwrap();

        let resolved =
            resolve_adopted(r"music\Album\AC: DC - 01.mp3", &base, &HashSet::new()).unwrap();
        assert!(resolved.ends_with("AC: DC - 01.mp3"));

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn resolves_inside_the_batch_folder_and_skips_claimed_copies() {
        let base = temp_base("batch");
        let folder = base.join("F");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("01.flac"), b"x").unwrap();
        std::fs::write(folder.join("01_639097129778484198.flac"), b"x").unwrap();
        // Same name one folder up: outside the batch, never a candidate.
        std::fs::write(base.join("01.flac"), b"x").unwrap();

        let mut claimed = HashSet::new();
        let first = resolve_in_destination(r"music\A\01.flac", &base, "F", &claimed).unwrap();
        assert_eq!(first, folder.join("01.flac"));

        claimed.insert(file_id(&first).unwrap());
        let second = resolve_in_destination(r"other\B\01.flac", &base, "F", &claimed).unwrap();
        assert_eq!(second, folder.join("01_639097129778484198.flac"));

        claimed.insert(file_id(&second).unwrap());
        assert_eq!(
            resolve_in_destination(r"music\A\01.flac", &base, "F", &claimed),
            Err(Unresolved::Claimed)
        );

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_new_file_under_a_handed_off_name_resolves() {
        // beets moved the first 01.flac away, and the batch's second 01.flac
        // then landed under the free plain name.
        let base = temp_base("reused-name");
        let folder = base.join("F");
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("01.flac");
        std::fs::write(&path, b"disc 1").unwrap();
        let claimed = HashSet::from([file_id(&path).unwrap()]);
        assert_eq!(
            resolve_in_destination("01.flac", &base, "F", &claimed),
            Err(Unresolved::Claimed)
        );

        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"disc 2").unwrap();

        assert_eq!(
            resolve_in_destination("01.flac", &base, "F", &claimed),
            Ok(path)
        );

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn collision_copies_resolve_oldest_first_and_need_exact_ticks() {
        let base = temp_base("ticks-order");
        let folder = base.join("F");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("01_639097129778484199.flac"), b"x").unwrap();
        std::fs::write(folder.join("01_639097129778484198.flac"), b"x").unwrap();
        // Not slskd's pattern: wrong digit count, or a different track.
        std::fs::write(folder.join("01_123.flac"), b"x").unwrap();
        std::fs::write(folder.join("01_x.flac"), b"x").unwrap();

        let claimed = HashSet::new();
        assert_eq!(
            resolve_in_destination("01.flac", &base, "F", &claimed),
            Ok(folder.join("01_639097129778484198.flac"))
        );
        assert_eq!(
            resolve_in_destination("02.flac", &base, "F", &claimed),
            Err(Unresolved::NotOnDisk)
        );
        assert_eq!(
            resolve_in_destination("01.flac", &base, "missing", &claimed),
            Err(Unresolved::NotOnDisk)
        );

        std::fs::remove_dir_all(&base).unwrap();
    }

    /// What a backend that writes its own files names them is what resolve reads.
    #[test]
    fn a_file_landed_under_a_landing_name_or_a_ticks_copy_of_it_resolves() {
        use shared::download::{dotnet_ticks, ticks_copy};
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("F");
        std::fs::create_dir_all(&folder).unwrap();
        let remote = r"music\A\AC: DC - 01.flac";
        let names = landing_names("AC: DC - 01.flac");
        std::fs::write(folder.join(&names[0]), b"x").unwrap();
        let copy = ticks_copy(&names[0], dotnet_ticks(std::time::SystemTime::now()));
        std::fs::write(folder.join(&copy), b"y").unwrap();

        let mut claimed = HashSet::new();
        let plain = resolve_in_destination(remote, root.path(), "F", &claimed).unwrap();
        assert_eq!(plain, folder.join(&names[0]));
        claimed.insert(file_id(&plain).unwrap());
        assert_eq!(
            resolve_in_destination(remote, root.path(), "F", &claimed),
            Ok(folder.join(copy))
        );
    }

    #[test]
    fn resolves_collision_suffixed_file() {
        let base = temp_base("ticks");
        let album = base.join("Album");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("track_639097129778484198.mp3"), b"x").unwrap();

        let resolved = resolve_adopted(r"music\Album\track.mp3", &base, &HashSet::new()).unwrap();
        assert!(resolved.ends_with("track_639097129778484198.mp3"));

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn an_adopted_name_that_leaves_its_folder_names_no_file() {
        // The peer's name points at a file that exists outside the download
        // folder: an absolute path, a drive, or steps up through `..`.
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("downloads");
        let library = root.path().join("music/bob/Album");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&library).unwrap();
        std::fs::write(library.join("x.flac"), b"bob's").unwrap();
        // What strategy 1 would also find for a plain name of the same parent.
        std::fs::create_dir_all(base.join("Album")).unwrap();
        let none = HashSet::new();

        let absolute = format!("{}", library.join("x.flac").display());
        for name in [
            absolute.as_str(),
            r"..\music\bob\Album\x.flac",
            "../music/bob/Album/x.flac",
            r"Album\..\..\music\bob\Album\x.flac",
            r"C:\music\Album\x.flac",
            "./Album/x.flac",
            r"..\x.flac",
        ] {
            assert_eq!(
                resolve_adopted(name, &base, &none),
                Err(Unresolved::NotOnDisk),
                "{name}"
            );
        }
        assert!(library.join("x.flac").exists());
    }

    #[test]
    fn an_adopted_file_is_looked_for_only_where_slskd_saves_it() {
        // Another batch's folder holds a file of the same name: an adopted
        // transfer never takes it, nor one through a link out of the folder.
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("downloads");
        let other = base.join("Live [1a2b3c4d]");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("01 - Intro.flac"), b"bob's").unwrap();
        let none = HashSet::new();
        assert_eq!(
            resolve_adopted(r"music\Live\01 - Intro.flac", &base, &none),
            Err(Unresolved::NotOnDisk)
        );

        let outside = root.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("02.flac"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, base.join("Linked")).unwrap();
        assert_eq!(
            resolve_adopted(r"music\Linked\02.flac", &base, &none),
            Err(Unresolved::NotOnDisk)
        );

        std::fs::create_dir_all(base.join("Live")).unwrap();
        std::fs::write(base.join("Live/01 - Intro.flac"), b"mine").unwrap();
        assert_eq!(
            resolve_adopted(r"music\Live\01 - Intro.flac", &base, &none),
            Ok(base.join("Live/01 - Intro.flac"))
        );
        std::fs::write(base.join("03.flac"), b"x").unwrap();
        assert_eq!(
            resolve_adopted("03.flac", &base, &none),
            Ok(base.join("").join("03.flac"))
        );
    }
}
