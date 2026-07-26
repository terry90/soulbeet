#[cfg(feature = "server")]
use shared::slskd::{sanitize_filename, sanitize_filename_unix};
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

/// Extract the album directory from a resolved path.
/// This is used for grouping files by album for beets import.
///
/// # Arguments
/// * `resolved_path` - A resolved filesystem path to a downloaded file
///
/// # Returns
/// * The parent directory path (album directory)
#[cfg(feature = "server")]
pub fn get_album_directory(resolved_path: &str) -> Option<String> {
    let path = Path::new(resolved_path);
    path.parent().map(|p| p.to_string_lossy().to_string())
}

/// Check if two paths are in the same album directory.
/// This is used for grouping files together for album-mode import.
#[cfg(feature = "server")]
pub fn same_album_directory(path1: &str, path2: &str) -> bool {
    match (get_album_directory(path1), get_album_directory(path2)) {
        (Some(dir1), Some(dir2)) => dir1 == dir2,
        _ => false,
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
