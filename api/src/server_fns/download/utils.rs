#[cfg(feature = "server")]
use shared::slskd::{sanitize_filename, sanitize_filename_unix};
#[cfg(feature = "server")]
use std::collections::HashSet;
#[cfg(feature = "server")]
use std::path::{Path, PathBuf};
#[cfg(feature = "server")]
use tracing::warn;

/// Resolve the download path from a slskd filename to an actual filesystem path.
///
/// slskd receives remote Soulseek paths (e.g. `music\Artist\Album\track.flac`) and stores
/// downloads locally using only the last 2 path components: `<download_dir>/Album/track.flac`.
/// It also replaces invalid filename characters with `_` and may append a timestamp suffix
/// (e.g. `_639097129778484198`) to avoid collisions.
///
/// This function replicates slskd's path resolution logic and falls back to progressively
/// fuzzier matching strategies.
///
/// # Arguments
/// * `filename` - The slskd filename (remote Soulseek path, may contain Windows-style backslashes)
/// * `download_base` - The base download directory
///
/// # Returns
/// * `Some(path)` - The resolved path if the file exists
/// * `None` - If the file cannot be found
#[cfg(feature = "server")]
pub fn resolve_download_path(filename: &str, download_base: &Path) -> Option<String> {
    // Normalize path separators (Windows -> Unix)
    let filename_str = filename.replace('\\', "/");
    let path = Path::new(&filename_str);
    let components: Vec<&str> = filename_str.split('/').filter(|s| !s.is_empty()).collect();

    if components.is_empty() {
        warn!("Empty filename provided for path resolution");
        return None;
    }

    // Strategy 1: the layout Soulful pins at enqueue time (batch destination =
    // sanitized parent directory), which is also slskd's default pattern:
    // <downloads>/<directory>/<filename>. The basename is sanitized by slskd
    // with its host OS's invalid set, so both variants are tried.
    if components.len() >= 2 {
        let dir_part = sanitize_filename(components[components.len() - 2]);
        let album_dir = download_base.join(&dir_part);
        for file_part in basename_candidates(components[components.len() - 1]) {
            let candidate = album_dir.join(&file_part);
            if candidate.exists() {
                return Some(candidate.to_string_lossy().to_string());
            }

            // slskd appends _<ticks> when a file with the same name already exists.
            // Search the expected album directory for files whose stem starts with ours.
            if let Some(found) = find_file_by_stem(&album_dir, &file_part) {
                return Some(found.to_string_lossy().to_string());
            }
        }
    } else {
        // Single component (just a filename)
        for file_part in basename_candidates(components[0]) {
            let candidate = download_base.join(&file_part);
            if candidate.exists() {
                return Some(candidate.to_string_lossy().to_string());
            }
        }
    }

    // Strategy 2: Try the full path relative to download base (handles setups
    // where slskd preserves the full remote directory structure)
    let full_relative = download_base.join(&filename_str);
    if full_relative.exists() {
        return Some(full_relative.to_string_lossy().to_string());
    }

    // Strategy 3: Try with @@username prefix stripped
    if let Some(first) = components.first() {
        if first.starts_with("@@") && components.len() > 1 {
            let without_user = components[1..].join("/");
            let candidate = download_base.join(&without_user);
            if candidate.exists() {
                return Some(candidate.to_string_lossy().to_string());
            }
        }
    }

    // Strategy 4: Try last 3 components (artist/album/track)
    if components.len() >= 3 {
        let len = components.len();
        let three_level = components[len - 3..].join("/");
        let candidate = download_base.join(&three_level);
        if candidate.exists() {
            return Some(candidate.to_string_lossy().to_string());
        }
    }

    // Strategy 5: Recursive search with fuzzy stem matching
    // Handles unusual path structures and collision-suffixed filenames
    if let Some(file_name) = path.file_name() {
        let file_name_str = file_name.to_string_lossy();
        let sanitized = sanitize_filename(&file_name_str);
        if let Some(found) = find_file_recursive_fuzzy(download_base, &sanitized) {
            return Some(found.to_string_lossy().to_string());
        }
    }

    // Could not find the file
    warn!(
        "Could not resolve download path for '{}' in '{}'",
        filename,
        download_base.display()
    );
    None
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

/// Find a finished download in its batch folder `base/destination`.
///
/// slskd keeps the remote basename (sanitized) and, when that name is already
/// taken, writes `<stem>_<ticks><ext>` with 18-digit ticks. Two files of one
/// batch with the same basename therefore land as the plain name and its
/// copies. The plain name is tried first, then the copies oldest first, and
/// files in `exclude` (already handed off) are skipped.
#[cfg(feature = "server")]
pub fn resolve_in_destination(
    filename: &str,
    base: &Path,
    destination: &str,
    exclude: &HashSet<FileId>,
) -> Option<PathBuf> {
    let normalized = filename.replace('\\', "/");
    let remote_name = normalized.rsplit('/').find(|s| !s.is_empty())?;
    let folder = base.join(destination);
    let names = basename_candidates(remote_name);

    let usable =
        |path: &PathBuf| path.is_file() && file_id(path).is_some_and(|id| !exclude.contains(&id));
    if let Some(plain) = names.iter().map(|n| folder.join(n)).find(usable) {
        return Some(plain);
    }

    let mut copies: Vec<PathBuf> = std::fs::read_dir(&folder)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| names.iter().any(|want| is_ticks_copy(n, want)))
        })
        .filter(usable)
        .collect();
    // Equal-width ticks, so name order is age order.
    copies.sort();
    copies.into_iter().next()
}

/// Whether `name` is slskd's renamed copy of `original`: `<stem>_<18 digits><ext>`.
#[cfg(feature = "server")]
fn is_ticks_copy(name: &str, original: &str) -> bool {
    let original = Path::new(original);
    let Some(stem) = original.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    let ext = original
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    name.strip_prefix(stem)
        .and_then(|rest| rest.strip_prefix('_'))
        .and_then(|rest| rest.strip_suffix(ext.as_str()))
        .is_some_and(|ticks| ticks.len() == 18 && ticks.bytes().all(|b| b.is_ascii_digit()))
}

/// The on-disk basename variants slskd may have produced for a remote
/// filename: sanitized with the Windows-invalid set (slskd on Windows, and
/// what Soulful's own destinations use) or with the Unix set (slskd on
/// Linux keeps characters like `:`). Deduplicated when they agree.
#[cfg(feature = "server")]
fn basename_candidates(name: &str) -> Vec<String> {
    let windows = sanitize_filename(name);
    let unix = sanitize_filename_unix(name);
    if unix == windows {
        vec![windows]
    } else {
        vec![windows, unix]
    }
}

/// Search a specific directory for a file whose stem matches the expected filename,
/// allowing for slskd's collision-avoidance suffix (_<ticks> before the extension).
///
/// For example, if we expect `track.flac`, this will also match `track_639097129778484198.flac`.
#[cfg(feature = "server")]
fn find_file_by_stem(dir: &Path, expected_filename: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;

    let expected_stem = Path::new(expected_filename)
        .file_stem()?
        .to_string_lossy()
        .to_lowercase();
    let expected_ext = Path::new(expected_filename)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase());

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let name = path.file_name()?.to_string_lossy().to_lowercase();

        // Exact match (case-insensitive)
        if name == expected_filename.to_lowercase() {
            return Some(path);
        }

        // Check for collision suffix: stem starts with expected stem, same extension
        let file_stem = path.file_stem()?.to_string_lossy().to_lowercase();
        let file_ext = path.extension().map(|e| e.to_string_lossy().to_lowercase());

        if file_ext == expected_ext
            && file_stem.starts_with(&*expected_stem)
            && file_stem[expected_stem.len()..].starts_with('_')
        {
            // Verify the suffix after the underscore is numeric (ticks)
            let suffix = &file_stem[expected_stem.len() + 1..];
            if suffix.chars().all(|c| c.is_ascii_digit()) {
                return Some(path);
            }
        }
    }

    None
}

/// Recursively search for a file, matching by stem prefix to handle collision suffixes.
#[cfg(feature = "server")]
fn find_file_recursive_fuzzy(dir: &Path, expected_filename: &str) -> Option<PathBuf> {
    const MAX_DEPTH: usize = 5;

    fn search(dir: &Path, expected_filename: &str, depth: usize) -> Option<PathBuf> {
        if depth > MAX_DEPTH {
            return None;
        }

        // First try stem-based matching in this directory
        if let Some(found) = find_file_by_stem(dir, expected_filename) {
            return Some(found);
        }

        // Recurse into subdirectories
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return None,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = search(&path, expected_filename, depth + 1) {
                    return Some(found);
                }
            }
        }

        None
    }

    search(dir, expected_filename, 0)
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

        let resolved = resolve_download_path(r"music\Album\AC: DC - 01.mp3", &base).unwrap();
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

        let resolved = resolve_download_path(r"music\Album\AC: DC - 01.mp3", &base).unwrap();
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
            None
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
            None
        );

        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"disc 2").unwrap();

        assert_eq!(
            resolve_in_destination("01.flac", &base, "F", &claimed),
            Some(path)
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
            Some(folder.join("01_639097129778484198.flac"))
        );
        assert_eq!(
            resolve_in_destination("02.flac", &base, "F", &claimed),
            None
        );

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn resolves_collision_suffixed_file() {
        let base = temp_base("ticks");
        let album = base.join("Album");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("track_639097129778484198.mp3"), b"x").unwrap();

        let resolved = resolve_download_path(r"music\Album\track.mp3", &base).unwrap();
        assert!(resolved.ends_with("track_639097129778484198.mp3"));

        std::fs::remove_dir_all(&base).unwrap();
    }
}
