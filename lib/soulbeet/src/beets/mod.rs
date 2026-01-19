pub use shared::library::{DuplicateGroup, DuplicateReport, LibraryTrack};
use std::{collections::HashMap, path::Path, time::Duration};
use tokio::process::Command;
use tracing::{info, warn};

/// Timeout for beets import process (5 minutes)
const IMPORT_TIMEOUT_SECS: u64 = 300;

/// Result of a beets import operation
#[derive(Debug)]
pub enum ImportResult {
    /// Import completed successfully
    Success,
    /// Import was skipped (e.g., duplicate detection)
    Skipped,
    /// Import failed with an error message
    Failed(String),
    /// Import timed out
    TimedOut,
}

/// Error type for import operations
#[derive(Debug)]
pub enum ImportError {
    /// IO error (process spawn failed, etc.)
    Io(std::io::Error),
    /// Import timed out
    Timeout,
    /// Source path validation failed
    InvalidSource(String),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::Io(e) => write!(f, "IO error: {}", e),
            ImportError::Timeout => write!(f, "Import timed out after {}s", IMPORT_TIMEOUT_SECS),
            ImportError::InvalidSource(path) => write!(f, "Invalid source path: {}", path),
        }
    }
}

impl std::error::Error for ImportError {}

impl From<std::io::Error> for ImportError {
    fn from(e: std::io::Error) -> Self {
        ImportError::Io(e)
    }
}

/// Validate that source paths exist before attempting import
fn validate_sources(sources: &[String]) -> Result<(), ImportError> {
    for source in sources {
        let path = Path::new(source);
        if !path.exists() {
            return Err(ImportError::InvalidSource(format!(
                "Source path does not exist: {}",
                source
            )));
        }
        if !path.is_file() && !path.is_dir() {
            return Err(ImportError::InvalidSource(format!(
                "Source path is neither a file nor directory: {}",
                source
            )));
        }
    }
    Ok(())
}

/// Import music files using beets
///
/// # Arguments
/// * `sources` - List of source file/directory paths to import
/// * `target` - Target directory for the music library
/// * `as_album` - If true, import as album; if false, import as singletons
///
/// # Returns
/// * `Ok(ImportResult)` - The result of the import operation
/// * `Err(ImportError)` - If the import failed to execute
pub async fn import(
    sources: Vec<String>,
    target: &Path,
    as_album: bool,
) -> Result<ImportResult, ImportError> {
    // Validate sources exist before attempting import
    validate_sources(&sources)?;

    let config_path =
        std::env::var("BEETS_CONFIG").unwrap_or_else(|_| "beets_config.yaml".to_string());

    info!(
        "Starting beet import for {} items to {:?} using config {} (album mode: {})",
        sources.len(),
        target,
        config_path,
        as_album
    );

    // Use a library database specific to this target directory for duplicate detection
    let library_path = target.join(".beets_library.db");

    let mut cmd = Command::new("beet");
    cmd.arg("-c")
        .arg(&config_path)
        .arg("-l") // library database path (for duplicate detection)
        .arg(&library_path)
        .arg("-d") // destination directory
        .arg(target)
        .arg("import")
        .arg("-q"); // quiet mode: do not ask for confirmation

    if !as_album {
        cmd.arg("-s"); // singleton mode
    }

    for source in &sources {
        cmd.arg(source);
    }

    // Execute with timeout to prevent hanging
    let output = match tokio::time::timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS), cmd.output())
        .await
    {
        Ok(result) => result?,
        Err(_) => {
            warn!(
                "Beet import timed out after {}s for sources: {:?}",
                IMPORT_TIMEOUT_SECS, sources
            );
            return Ok(ImportResult::TimedOut);
        }
    };

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Check both stdout and stderr for skip indicators
        // Beets may output to either depending on version/config
        let output_combined = format!("{}{}", stdout, stderr);

        if output_combined.to_lowercase().contains("skipping")
            || output_combined.to_lowercase().contains("skip")
        {
            info!("Beet import skipped items");
            Ok(ImportResult::Skipped)
        } else {
            info!("Beet import successful");
            Ok(ImportResult::Success)
        }
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);

        // Combine both streams for error reporting since beets can be inconsistent
        let error_msg = if stderr.is_empty() {
            if stdout.is_empty() {
                format!("Beet import failed with exit code: {:?}", output.status.code())
            } else {
                stdout.to_string()
            }
        } else {
            stderr.to_string()
        };

        info!("Beet import failed: {}", error_msg);
        Ok(ImportResult::Failed(error_msg))
    }
}

/// Query tracks from a beets library database
async fn query_library(library_path: &Path) -> Result<Vec<LibraryTrack>, String> {
    if !library_path.exists() {
        return Ok(Vec::new());
    }

    let config_path =
        std::env::var("BEETS_CONFIG").unwrap_or_else(|_| "beets_config.yaml".to_string());

    // Use beet ls with format to get track info
    // Format: path|artist|title|album|albumartist
    let output = Command::new("beet")
        .arg("-c")
        .arg(&config_path)
        .arg("-l")
        .arg(library_path)
        .arg("ls")
        .arg("-f")
        .arg("$path|||$artist|||$title|||$album|||$albumartist")
        .output()
        .await
        .map_err(|e| format!("Failed to query library: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Empty library is not an error
        if stderr.contains("no items") || stderr.is_empty() {
            return Ok(Vec::new());
        }
        return Err(format!("Beet ls failed: {}", stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let library_str = library_path.to_string_lossy().to_string();

    let tracks: Vec<LibraryTrack> = stdout
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split("|||").collect();
            if parts.len() >= 5 {
                Some(LibraryTrack {
                    path: parts[0].to_string(),
                    artist: parts[1].to_string(),
                    title: parts[2].to_string(),
                    album: parts[3].to_string(),
                    album_artist: parts[4].to_string(),
                    library_path: library_str.clone(),
                })
            } else {
                None
            }
        })
        .collect();

    Ok(tracks)
}

/// Find duplicate tracks across multiple library folders
///
/// # Arguments
/// * `library_paths` - List of library root directories to scan
///
/// # Returns
/// * `Ok(DuplicateReport)` - Report of all duplicates found
/// * `Err(String)` - If scanning failed
pub async fn find_duplicates_across_libraries(
    library_paths: Vec<&Path>,
) -> Result<DuplicateReport, String> {
    let mut all_tracks: Vec<LibraryTrack> = Vec::new();
    let mut libraries_scanned: Vec<String> = Vec::new();

    for lib_path in &library_paths {
        let db_path = lib_path.join(".beets_library.db");
        info!("Scanning library: {:?}", db_path);

        match query_library(&db_path).await {
            Ok(tracks) => {
                info!("Found {} tracks in {:?}", tracks.len(), lib_path);
                all_tracks.extend(tracks);
                libraries_scanned.push(lib_path.to_string_lossy().to_string());
            }
            Err(e) => {
                warn!("Failed to scan library {:?}: {}", lib_path, e);
            }
        }
    }

    // Group tracks by (artist, title) - normalized to lowercase for comparison
    let mut track_groups: HashMap<(String, String), Vec<LibraryTrack>> = HashMap::new();

    for track in all_tracks {
        let key = (
            track.artist.to_lowercase().trim().to_string(),
            track.title.to_lowercase().trim().to_string(),
        );
        track_groups.entry(key).or_default().push(track);
    }

    // Filter to only groups with tracks from different libraries
    let duplicates: Vec<DuplicateGroup> = track_groups
        .into_iter()
        .filter(|(_, tracks)| {
            // Check if tracks span multiple libraries
            let unique_libs: std::collections::HashSet<_> =
                tracks.iter().map(|t| &t.library_path).collect();
            unique_libs.len() > 1
        })
        .map(|((artist, title), tracks)| {
            // Use the original case from the first track for display
            let display_artist = tracks.first().map(|t| t.artist.clone()).unwrap_or(artist);
            let display_title = tracks.first().map(|t| t.title.clone()).unwrap_or(title);
            DuplicateGroup {
                artist: display_artist,
                title: display_title,
                tracks,
            }
        })
        .collect();

    let total_duplicate_tracks: usize = duplicates.iter().map(|g| g.tracks.len()).sum();

    Ok(DuplicateReport {
        duplicates,
        total_duplicate_tracks,
        libraries_scanned,
    })
}
