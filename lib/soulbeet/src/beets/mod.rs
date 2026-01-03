use std::{io::Result, path::Path};
use tokio::process::Command;
use tracing::info;

pub enum ImportResult {
    Success,
    Skipped,
    Failed(String),
}

pub async fn import(sources: Vec<String>, target: &Path, as_album: bool) -> Result<ImportResult> {
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

    for source in sources {
        cmd.arg(source);
    }

    let output = cmd.output().await?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.contains("Skipping") {
            info!("Beet import skipped items");
            Ok(ImportResult::Skipped)
        } else {
            info!("Beet import successful");
            Ok(ImportResult::Success)
        }
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        info!("Beet import failed: {}", stderr);
        Ok(ImportResult::Failed(stderr.to_string()))
    }
}
