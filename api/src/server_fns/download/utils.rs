#[cfg(feature = "server")]
use std::path::{Path, PathBuf};
#[cfg(feature = "server")]
use tracing::warn;

/// Resolve the download path from a slskd filename to an actual filesystem path.
///
/// slskd filenames typically look like: `@@username\path\to\artist\album\track.mp3`
/// We need to find the corresponding file in the download directory.
///
/// # Arguments
/// * `filename` - The slskd filename (may contain Windows-style backslashes)
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
    let components: Vec<_> = path.components().collect();

    if components.is_empty() {
        warn!("Empty filename provided for path resolution");
        return None;
    }

    // Try multiple strategies to find the file

    // Strategy 1: Try the full path relative to download base
    // This handles cases where slskd preserves the full path structure
    let full_relative = download_base.join(&filename_str);
    if full_relative.exists() {
        return Some(full_relative.to_string_lossy().to_string());
    }

    // Strategy 2: Try with username prefix stripped (@@username/path/to/file -> path/to/file)
    // slskd often prefixes with @@username
    if let Some(first) = components.first() {
        let first_str = first.as_os_str().to_string_lossy();
        if first_str.starts_with("@@") && components.len() > 1 {
            let without_user: PathBuf = components[1..].iter().collect();
            let candidate = download_base.join(&without_user);
            if candidate.exists() {
                return Some(candidate.to_string_lossy().to_string());
            }
        }
    }

    // Strategy 3: Try preserving album structure (last 2-3 components)
    // This is important for album grouping: artist/album/track.mp3
    if components.len() >= 3 {
        // Try last 3 components (artist/album/track)
        let len = components.len();
        let three_level: PathBuf = components[len - 3..].iter().collect();
        let candidate = download_base.join(&three_level);
        if candidate.exists() {
            return Some(candidate.to_string_lossy().to_string());
        }
    }

    // Strategy 4: Try last 2 components (album/track)
    if components.len() >= 2 {
        let len = components.len();
        let two_level: PathBuf = components[len - 2..].iter().collect();
        let candidate = download_base.join(&two_level);
        if candidate.exists() {
            return Some(candidate.to_string_lossy().to_string());
        }
    }

    // Strategy 5: Try just the filename
    if let Some(file_name) = components.last() {
        let candidate = download_base.join(file_name);
        if candidate.exists() {
            return Some(candidate.to_string_lossy().to_string());
        }
    }

    // Strategy 6: Search recursively in download directory for the filename
    // This is a fallback for unusual path structures
    if let Some(file_name) = path.file_name() {
        if let Some(found) = find_file_recursive(download_base, file_name) {
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

/// Recursively search for a file in a directory (limited depth to avoid excessive searching)
#[cfg(feature = "server")]
fn find_file_recursive(dir: &Path, target_name: &std::ffi::OsStr) -> Option<PathBuf> {
    const MAX_DEPTH: usize = 5;

    fn search(dir: &Path, target_name: &std::ffi::OsStr, depth: usize) -> Option<PathBuf> {
        if depth > MAX_DEPTH {
            return None;
        }

        let entries = std::fs::read_dir(dir).ok()?;

        for entry in entries.flatten() {
            let path = entry.path();

            if path.is_file() {
                if let Some(name) = path.file_name() {
                    // Case-insensitive comparison for cross-platform compatibility
                    if name.to_string_lossy().to_lowercase()
                        == target_name.to_string_lossy().to_lowercase()
                    {
                        return Some(path);
                    }
                }
            } else if path.is_dir() {
                if let Some(found) = search(&path, target_name, depth + 1) {
                    return Some(found);
                }
            }
        }

        None
    }

    search(dir, target_name, 0)
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
