use dioxus::prelude::*;
use shared::navidrome::{
    BlockedDeletion, DeletionReview, LibraryStats, SyncResult, UnreadableListener,
};

#[cfg(feature = "server")]
use super::{failure_error, server_error};
#[cfg(feature = "server")]
use crate::core::database_failure;
#[cfg(feature = "server")]
use crate::core::index::ServerNavidrome;
#[cfg(feature = "server")]
use crate::core::library;
#[cfg(feature = "server")]
use crate::core::ratings::{self, AutoDeletePermit};
#[cfg(feature = "server")]
use crate::models::deletion_block::DeletionBlockRow;
#[cfg(feature = "server")]
use crate::models::deletion_review::DeletionReviewRow;
#[cfg(feature = "server")]
use crate::models::navidrome_read_failure::NavidromeReadFailureRow;
#[cfg(feature = "server")]
use crate::services::navidrome_client_for_user;
#[cfg(feature = "server")]
use crate::{AdminSession, AuthSession};
#[cfg(feature = "server")]
use shared::failure::Stage;

#[post("/api/navidrome/sync-ratings", auth: AuthSession)]
pub async fn sync_ratings() -> Result<SyncResult, ServerFnError> {
    let report =
        (ratings::sync(&auth.0, AutoDeletePermit::Allowed).await).map_err(failure_error)?;
    // SyncResult has no room for it, and a sync whose kept tracks Settings does not list did
    // not succeed.
    match report.kept_not_stored {
        Some(f) => Err(failure_error(f)),
        None => Ok(report.counts()),
    }
}

#[get("/api/navidrome/deletion-history", auth: AuthSession)]
pub async fn get_deletion_history() -> Result<Vec<DeletionReview>, ServerFnError> {
    DeletionReviewRow::history(&auth.0.user_id, 50)
        .await
        .map_err(server_error)
}

/// The tracks auto-delete kept at the caller's last rating sync, and why.
#[get("/api/navidrome/deletion-blocks", auth: AuthSession)]
pub async fn get_deletion_blocks() -> Result<Vec<BlockedDeletion>, ServerFnError> {
    DeletionBlockRow::for_user_in(&crate::db::DB, &auth.0.user_id)
        .await
        .map_err(|e| failure_error(database_failure(Stage::Delete, e)))
}

/// The accounts the delete guard cannot read, which keeps every file they might have rated.
#[get("/api/navidrome/unreadable-listeners", _: AdminSession)]
pub async fn get_unreadable_listeners() -> Result<Vec<UnreadableListener>, ServerFnError> {
    NavidromeReadFailureRow::all_in(&crate::db::DB)
        .await
        .map_err(|e| failure_error(database_failure(Stage::Delete, e)))
}

/// Check if the Soulbeet player in Navidrome has ReportRealPath enabled.
/// Returns Some(true) if enabled, Some(false) if disabled, None if player not found yet.
#[cfg(feature = "server")]
pub async fn check_report_real_path(user_id: &str) -> Result<Option<bool>, String> {
    let client = navidrome_client_for_user(user_id).await?;
    let players = client.get_players().await.map_err(|e| e.to_string())?;
    let soulbeet = players.iter().find(|p| p.client == "Soulbeet");
    Ok(soulbeet.map(|p| p.report_real_path))
}

#[get("/api/navidrome/library-stats", auth: AuthSession)]
pub async fn get_library_stats() -> Result<LibraryStats, ServerFnError> {
    let client = library::navidrome_for(&ServerNavidrome, &auth.0, Stage::LibraryCheck)
        .await
        .map_err(failure_error)?;
    library::stats(&client).await.map_err(failure_error)
}
