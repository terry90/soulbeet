use dioxus::prelude::*;
use shared::playlist_import::{
    CommitPlaylistImportRequest, CommitPlaylistImportResponse, PlaylistImportPreview,
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
