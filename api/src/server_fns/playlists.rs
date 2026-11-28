use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[cfg(feature = "server")]
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};

#[cfg(feature = "server")]
use crate::{
    core::{
        index::ServerNavidrome,
        library::navidrome_for,
        playlists::{self, MetaChange, NewPlaylist, Written, SONG_IDS_MAX},
    },
    server_fns::failure_error,
    AuthSession,
};

/// Summary of a Navidrome playlist returned after a playlist mutation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaylistMutationResult {
    pub id: String,
    pub name: String,
    pub comment: String,
    pub public: bool,
    pub song_count: u32,
    pub duration: u32,
}

#[cfg(feature = "server")]
const MAX_PLAYLIST_NAME_CHARS: usize = 255;

#[cfg(feature = "server")]
const MAX_PLAYLIST_COMMENT_CHARS: usize = 2000;

#[cfg(feature = "server")]
fn invalid_input(msg: impl Into<String>) -> Failure {
    Failure::new(
        FailureCode::InvalidInput,
        Stage::Input,
        RetryAdvice::NotRetryable,
        msg,
    )
}

#[cfg(feature = "server")]
fn validate_playlist_id(playlist_id: &str) -> Result<&str, Failure> {
    let trimmed = playlist_id.trim();
    if trimmed.is_empty() {
        return Err(invalid_input("Playlist ID cannot be empty."));
    }
    Ok(trimmed)
}

#[cfg(feature = "server")]
fn validate_playlist_name(name: &str) -> Result<String, Failure> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(invalid_input("Playlist name cannot be empty."));
    }
    if trimmed.chars().count() > MAX_PLAYLIST_NAME_CHARS {
        return Err(invalid_input(format!(
            "Playlist name must be {MAX_PLAYLIST_NAME_CHARS} characters or fewer."
        )));
    }
    Ok(trimmed.to_string())
}

#[cfg(feature = "server")]
fn validate_comment(comment: Option<String>) -> Result<Option<String>, Failure> {
    if let Some(ref c) = comment {
        if c.chars().count() > MAX_PLAYLIST_COMMENT_CHARS {
            return Err(invalid_input(format!(
                "Playlist comment must be {MAX_PLAYLIST_COMMENT_CHARS} characters or fewer."
            )));
        }
    }
    Ok(comment)
}

#[cfg(feature = "server")]
fn validate_song_ids(song_ids: &[String]) -> Result<(), Failure> {
    if song_ids.len() > SONG_IDS_MAX {
        return Err(invalid_input(format!(
            "A playlist write can take at most {SONG_IDS_MAX} songs at once."
        )));
    }
    Ok(())
}

#[cfg(feature = "server")]
fn result_from_written(written: Written) -> PlaylistMutationResult {
    PlaylistMutationResult {
        id: written.playlist.playlist_id,
        name: written.playlist.name,
        comment: written.playlist.comment.unwrap_or_default(),
        public: written.playlist.public,
        song_count: written.playlist.song_count,
        duration: written.playlist.duration_s,
    }
}

#[post("/api/playlists/create", auth: AuthSession)]
pub async fn create_playlist(
    name: String,
    comment: Option<String>,
    public: Option<bool>,
    song_ids: Option<Vec<String>>,
) -> Result<PlaylistMutationResult, ServerFnError> {
    let name = validate_playlist_name(&name).map_err(failure_error)?;
    let comment = validate_comment(comment).map_err(failure_error)?;
    let song_ids = song_ids.unwrap_or_default();
    validate_song_ids(&song_ids).map_err(failure_error)?;

    let navi = navidrome_for(&ServerNavidrome, &auth.0, Stage::Playlist)
        .await
        .map_err(failure_error)?;
    let written = playlists::create(
        &navi,
        &auth.0,
        &NewPlaylist {
            name,
            comment,
            public,
            song_ids,
            allow_duplicate_name: false,
        },
    )
    .await
    .map_err(|r| failure_error(r.failure()))?;
    Ok(result_from_written(written))
}

#[post("/api/playlists/update", auth: AuthSession)]
pub async fn update_playlist(
    playlist_id: String,
    name: Option<String>,
    comment: Option<String>,
    public: Option<bool>,
) -> Result<PlaylistMutationResult, ServerFnError> {
    let playlist_id = validate_playlist_id(&playlist_id).map_err(failure_error)?;
    let name = match name {
        Some(n) => Some(validate_playlist_name(&n).map_err(failure_error)?),
        None => None,
    };
    let comment = validate_comment(comment).map_err(failure_error)?;

    let navi = navidrome_for(&ServerNavidrome, &auth.0, Stage::Playlist)
        .await
        .map_err(failure_error)?;
    let written = playlists::update(
        &navi,
        &auth.0,
        playlist_id,
        &MetaChange {
            name,
            comment,
            public,
            rules: None,
            allow_duplicate_name: false,
        },
    )
    .await
    .map_err(|r| failure_error(r.failure()))?;
    Ok(result_from_written(written))
}

#[post("/api/playlists/delete", auth: AuthSession)]
pub async fn delete_playlist(playlist_id: String) -> Result<(), ServerFnError> {
    let playlist_id = validate_playlist_id(&playlist_id).map_err(failure_error)?;
    let navi = navidrome_for(&ServerNavidrome, &auth.0, Stage::Playlist)
        .await
        .map_err(failure_error)?;
    playlists::delete(&navi, &auth.0, playlist_id)
        .await
        .map_err(|r| failure_error(r.failure()))?;
    Ok(())
}

#[post("/api/playlists/add-tracks", auth: AuthSession)]
pub async fn add_playlist_tracks(
    playlist_id: String,
    song_ids: Vec<String>,
) -> Result<PlaylistMutationResult, ServerFnError> {
    let playlist_id = validate_playlist_id(&playlist_id).map_err(failure_error)?;
    validate_song_ids(&song_ids).map_err(failure_error)?;
    let navi = navidrome_for(&ServerNavidrome, &auth.0, Stage::Playlist)
        .await
        .map_err(failure_error)?;
    let written = playlists::add_tracks(&navi, &auth.0, playlist_id, &song_ids, None)
        .await
        .map_err(|r| failure_error(r.failure()))?;
    Ok(result_from_written(written))
}

#[post("/api/playlists/remove-tracks", auth: AuthSession)]
pub async fn remove_playlist_tracks(
    playlist_id: String,
    song_ids: Vec<String>,
) -> Result<PlaylistMutationResult, ServerFnError> {
    let playlist_id = validate_playlist_id(&playlist_id).map_err(failure_error)?;
    validate_song_ids(&song_ids).map_err(failure_error)?;
    let navi = navidrome_for(&ServerNavidrome, &auth.0, Stage::Playlist)
        .await
        .map_err(failure_error)?;
    let written = playlists::remove_song_ids(&navi, &auth.0, playlist_id, &song_ids)
        .await
        .map_err(|r| failure_error(r.failure()))?;
    Ok(result_from_written(written))
}

#[post("/api/playlists/set-tracks", auth: AuthSession)]
pub async fn set_playlist_tracks(
    playlist_id: String,
    song_ids: Vec<String>,
) -> Result<PlaylistMutationResult, ServerFnError> {
    let playlist_id = validate_playlist_id(&playlist_id).map_err(failure_error)?;
    validate_song_ids(&song_ids).map_err(failure_error)?;
    let navi = navidrome_for(&ServerNavidrome, &auth.0, Stage::Playlist)
        .await
        .map_err(failure_error)?;
    let written = playlists::set_tracks(&navi, &auth.0, playlist_id, &song_ids)
        .await
        .map_err(|r| failure_error(r.failure()))?;
    Ok(result_from_written(written))
}
