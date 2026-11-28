use dioxus::prelude::*;
use shared::playlist_import::{
    CommitPlaylistImportRequest, CommitPlaylistImportResponse, ImportJobSummary,
    PlaylistImportPreview,
};

#[cfg(feature = "server")]
use crate::{server_fns::failure_error, AuthSession};

/// Resolves an external playlist URL (Spotify, YouTube Music, Deezer, or ListenBrainz)
/// and checks each track against the user's Navidrome library.
#[post("/api/playlists/import/preview", auth: AuthSession)]
pub async fn preview_playlist_import(url: String) -> Result<PlaylistImportPreview, ServerFnError> {
    crate::core::playlists::import::preview_playlist_import(
        &crate::core::index::ServerNavidrome,
        &auth.0,
        &url,
    )
    .await
    .map_err(failure_error)
}

/// Creates or appends to a Navidrome playlist with already-owned tracks and queues missing
/// tracks across enabled download backends.
#[post("/api/playlists/import/commit", auth: AuthSession)]
pub async fn commit_playlist_import(
    req: CommitPlaylistImportRequest,
) -> Result<CommitPlaylistImportResponse, ServerFnError> {
    crate::core::playlists::import::commit_playlist_import(
        &crate::core::jobs::runner::ACQUIRE,
        &crate::core::index::ServerNavidrome,
        &auth.0,
        req,
    )
    .await
    .map_err(failure_error)
}

/// Lists the caller's active and recent (last 24h) playlist import jobs (`BatchOrigin::Import`),
/// including selectable source candidates for jobs waiting in `needs_source_choice`.
#[get("/api/playlists/import/jobs", auth: AuthSession)]
pub async fn list_import_jobs() -> Result<Vec<ImportJobSummary>, ServerFnError> {
    crate::core::playlists::import::list_import_jobs(
        &crate::core::jobs::runner::ACQUIRE,
        &auth.0,
    )
    .await
    .map_err(failure_error)
}

/// Resolves a playlist import job waiting for source choice: downloads the chosen candidate
/// (`Some(candidate_index)`) or skips/cancels the job (`None`).
#[post("/api/playlists/import/jobs/resolve", auth: AuthSession)]
pub async fn resolve_import_job_choice(
    job_id: String,
    candidate_index: Option<usize>,
) -> Result<(), ServerFnError> {
    crate::core::playlists::import::resolve_import_job_choice(
        &crate::core::jobs::runner::ACQUIRE,
        &auth.0,
        &job_id,
        candidate_index,
    )
    .await
    .map_err(failure_error)
}

/// Clears all finished (`done`, `failed`, `cancelled`, `dismissed`) playlist import jobs for
/// the signed-in user and returns the number of jobs removed.
#[post("/api/playlists/import/jobs/clear-finished", auth: AuthSession)]
pub async fn clear_finished_import_jobs() -> Result<u64, ServerFnError> {
    crate::core::playlists::import::clear_finished_import_jobs(
        &crate::core::jobs::runner::ACQUIRE,
        &auth.0,
    )
    .await
    .map_err(failure_error)
}

/// Retries a failed or cancelled playlist import job (trying the next untried candidate or a
/// fresh search).
#[post("/api/playlists/import/jobs/retry", auth: AuthSession)]
pub async fn retry_import_job(job_id: String) -> Result<(), ServerFnError> {
    crate::core::playlists::import::retry_import_job(
        &crate::core::jobs::runner::ACQUIRE,
        &auth.0,
        &job_id,
    )
    .await
    .map_err(failure_error)
}
