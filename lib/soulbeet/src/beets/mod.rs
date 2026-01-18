use std::{path::Path, time::Duration};
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

    let mut cmd = Command::new("beet");
    cmd.arg("-c")
        .arg(&config_path)
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
