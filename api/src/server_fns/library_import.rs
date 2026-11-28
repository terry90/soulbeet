use dioxus::prelude::*;
use shared::library_import::{
    LibraryImportBatch, LibraryImportBatchDetail, LibraryImportItemStatus, LibraryImportMode,
    LibraryImportResolveAction, LibraryScanPreview,
};

#[cfg(feature = "server")]
use super::failure_error;
#[cfg(feature = "server")]
use crate::AuthSession;
#[cfg(feature = "server")]
use shared::library_import::LibraryImportOrigin;

/// Scans the caller's personal library folder (or a validated external source folder)
/// against the folder's beets database and returns a read-only preview of unimported units.
#[post("/api/library-import/scan", auth: AuthSession)]
pub async fn scan_unimported_files(
    source_path: Option<String>,
) -> Result<LibraryScanPreview, ServerFnError> {
    crate::core::library_import::scan_preview(&auth.0, source_path)
        .await
        .map_err(failure_error)
}

/// Starts a persistent background library import batch in the chosen mode.
#[post("/api/library-import/start", auth: AuthSession)]
pub async fn start_library_import(
    source_path: Option<String>,
    mode: LibraryImportMode,
) -> Result<LibraryImportBatch, ServerFnError> {
    crate::core::library_import::start_batch(&auth.0, source_path, mode, LibraryImportOrigin::Ui)
        .await
        .map_err(failure_error)
}

/// Lists the caller's recent library import batches, newest first.
#[get("/api/library-import/batches", auth: AuthSession)]
pub async fn list_library_import_batches() -> Result<Vec<LibraryImportBatch>, ServerFnError> {
    crate::core::library_import::list_batches(&auth.0)
        .await
        .map_err(failure_error)
}

/// Returns a library import batch with a paginated, optionally status-filtered slice of items.
#[get("/api/library-import/batches/{id}?status&limit&offset", auth: AuthSession)]
pub async fn get_library_import_batch(
    id: String,
    status: Option<LibraryImportItemStatus>,
    limit: Option<u32>,
    offset: Option<u32>,
) -> Result<LibraryImportBatchDetail, ServerFnError> {
    crate::core::library_import::get_batch_detail(
        &auth.0,
        &id,
        status,
        limit.unwrap_or(50),
        offset.unwrap_or(0),
    )
    .await
    .map_err(failure_error)
}

/// Pauses an active library import batch after the current unit finishes.
#[post("/api/library-import/batches/{id}/pause", auth: AuthSession)]
pub async fn pause_library_import(id: String) -> Result<LibraryImportBatch, ServerFnError> {
    crate::core::library_import::pause_batch(&auth.0, &id)
        .await
        .map_err(failure_error)
}

/// Resumes a paused library import batch.
#[post("/api/library-import/batches/{id}/resume", auth: AuthSession)]
pub async fn resume_library_import(id: String) -> Result<LibraryImportBatch, ServerFnError> {
    crate::core::library_import::resume_batch(&auth.0, &id)
        .await
        .map_err(failure_error)
}

/// Cancels a queued, running, or paused library import batch without touching any source files.
#[post("/api/library-import/batches/{id}/cancel", auth: AuthSession)]
pub async fn cancel_library_import(id: String) -> Result<LibraryImportBatch, ServerFnError> {
    crate::core::library_import::cancel_batch(&auth.0, &id)
        .await
        .map_err(failure_error)
}

/// Resolves a `needs_decision` or `failed` item via a Match Picker choice or dismisses it.
#[post("/api/library-import/items/{item_id}/resolve", auth: AuthSession)]
pub async fn resolve_library_import_item(
    item_id: String,
    action: LibraryImportResolveAction,
) -> Result<LibraryImportBatchDetail, ServerFnError> {
    crate::core::library_import::resolve_item(&auth.0, &item_id, action)
        .await
        .map_err(failure_error)
}
