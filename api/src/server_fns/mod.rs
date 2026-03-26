use dioxus::prelude::*;

pub mod auth;
pub mod discovery;
pub mod download;
pub mod folder;
pub mod guard;
pub mod navidrome;
pub mod search;
pub mod settings;
pub mod system;
pub mod user;

pub use auth::*;
pub use discovery::*;
pub use download::*;
pub use folder::*;
pub use guard::*;
pub use navidrome::*;
pub use search::*;
pub use settings::*;
pub use system::*;
pub use user::*;

pub fn server_error<E: std::fmt::Display>(e: E) -> ServerFnError {
    ServerFnError::ServerError {
        message: e.to_string(),
        code: 500,
        details: None,
    }
}

/// Remove a directory if empty, then recurse upward to its parent.
/// Stops at Discovery profile directories and beets library roots.
#[cfg(feature = "server")]
pub async fn cleanup_empty_ancestors(dir: &std::path::Path) -> Result<(), std::io::Error> {
    let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if matches!(
        dir_name,
        "Discovery" | "Conservative" | "Balanced" | "Adventurous"
    ) || dir.join(".beets_library.db").exists()
    {
        return Ok(());
    }
    let mut read_dir = tokio::fs::read_dir(dir).await?;
    if read_dir.next_entry().await?.is_none() {
        tokio::fs::remove_dir(dir).await?;
        if let Some(parent) = dir.parent() {
            let _ = Box::pin(cleanup_empty_ancestors(parent)).await;
        }
    }
    Ok(())
}
