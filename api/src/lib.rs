pub mod auth;
pub mod db;

use std::sync::LazyLock;

use auth::{AuthResponse, Claims};
use chrono::Duration;
use dioxus::{logger::tracing::info, prelude::*};
use serde::{Deserialize, Serialize};
use shared::{
    download::DownloadQuery,
    musicbrainz::{AlbumWithTracks, SearchResult},
    slskd::{AlbumResult, DownloadResponse, DownloadState, FileEntry, TrackResult},
};

#[cfg(feature = "server")]
use shared::musicbrainz::Track;
#[cfg(feature = "server")]
use soulful::beets;
#[cfg(feature = "server")]
use soulful::musicbrainz;
#[cfg(feature = "server")]
use soulful::slskd::{SoulseekClient, SoulseekClientBuilder};

#[cfg(feature = "server")]
static SLSKD_CLIENT: LazyLock<SoulseekClient> = LazyLock::new(|| {
    let api_key = std::env::var("SLSKD_API_KEY").expect("Missing SLSKD_API_KEY env var");
    let base_url = std::env::var("SLSKD_URL").expect("Missing SLSKD_URL env var");
    let download_path =
        std::env::var("SLSKD_DOWNLOAD_PATH").expect("Missing SLSKD_DOWNLOAD_PATH env var");

    SoulseekClientBuilder::new()
        .api_key(&api_key)
        .base_url(&base_url)
        .download_path(&download_path)
        .build()
        .expect("Failed to create Soulseek client")
});

fn server_error<E: std::fmt::Display>(e: E) -> ServerFnError {
    ServerFnError::ServerError {
        message: e.to_string(),
        code: 500,
        details: None,
    }
}

#[server]
pub async fn register(username: String, password: String) -> Result<(), ServerFnError> {
    db::User::create(&username, &password)
        .await
        .map_err(server_error)
        .map(|_| ())
}

#[server]
pub async fn login(username: String, password: String) -> Result<AuthResponse, ServerFnError> {
    let user = match db::User::verify(&username, &password).await {
        Ok(user) => user,
        Err(e) => return Err(server_error(e)),
    };

    auth::create_tokens(user.id, user.username).map_err(server_error)
}

#[server]
pub async fn refresh_token(token: String) -> Result<AuthResponse, ServerFnError> {
    let claims = match auth::verify_token(&token, "refresh") {
        Ok(c) => c,
        Err(e) => return Err(server_error(e)),
    };

    // In a real app, you might want to check if the user still exists or if the refresh token has been revoked

    auth::create_tokens(claims.sub, claims.username).map_err(server_error)
}

#[server]
pub async fn get_user_folders(token: String) -> Result<Vec<db::Folder>, ServerFnError> {
    let claims = match auth::verify_token(&token, "access") {
        Ok(c) => c,
        Err(e) => return Err(server_error(e)),
    };

    db::Folder::get_all_by_user(&claims.sub)
        .await
        .map_err(server_error)
}

#[server]
pub async fn create_user_folder(
    token: String,
    name: String,
    path: String,
) -> Result<db::Folder, ServerFnError> {
    let claims = match auth::verify_token(&token, "access") {
        Ok(c) => c,
        Err(e) => return Err(server_error(e)),
    };

    if let Err(e) = tokio::fs::create_dir_all(&path).await {
        return Err(server_error(format!("Failed to create directory: {}", e)));
    }

    db::Folder::create(&claims.sub, &name, &path)
        .await
        .map_err(server_error)
}

#[cfg(feature = "server")]
async fn slskd_search(
    artist: String,
    album: String,
    tracks: Vec<Track>,
) -> Result<Vec<AlbumResult>, ServerFnError> {
    let mut search = match SLSKD_CLIENT
        .search(artist, album, tracks, Duration::seconds(45))
        .await
    {
        Ok(s) => s,
        Err(e) => return Err(server_error(e)),
    };

    search.sort_by(|a, b| b.score.total_cmp(&a.score));

    for album in search.iter().take(10) {
        info!("Album: {}", album.album_title);
        info!("Score: {}", album.score);
        info!("Quality: {}", album.dominant_quality);

        for track in album.tracks.iter() {
            info!("  Filename: {:?}", track.base.filename);
            info!("  Title: {:?}", track.title);
            info!("  Artist: {:?}", track.artist);
            info!("  Album: {:?}", track.album);
            info!("  Format: {:?}", track.base.quality());
        }
    }

    Ok(search)
}

#[cfg(feature = "server")]
async fn slskd_download(tracks: Vec<TrackResult>) -> Result<Vec<DownloadResponse>, ServerFnError> {
    SLSKD_CLIENT.download(tracks).await.map_err(server_error)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchQuery {
    pub artist: Option<String>,
    pub query: String,
}

#[server]
pub async fn search_album(input: SearchQuery) -> Result<Vec<SearchResult>, ServerFnError> {
    musicbrainz::search(
        &input.artist,
        &input.query,
        musicbrainz::SearchType::Album,
        25,
    )
    .await
    .map_err(server_error)
}

#[server]
pub async fn search_track(input: SearchQuery) -> Result<Vec<SearchResult>, ServerFnError> {
    musicbrainz::search(
        &input.artist,
        &input.query,
        musicbrainz::SearchType::Track,
        25,
    )
    .await
    .map_err(server_error)
}

#[server]
pub async fn find_album(id: String) -> Result<AlbumWithTracks, ServerFnError> {
    musicbrainz::find_album(&id).await.map_err(server_error)
}

#[server]
pub async fn search_downloads(data: DownloadQuery) -> Result<Vec<AlbumResult>, ServerFnError> {
    slskd_search(data.album.artist, data.album.title, data.tracks).await
}

#[server]
pub async fn download(
    tracks: Vec<TrackResult>,
    target_folder: String,
) -> Result<Vec<DownloadResponse>, ServerFnError> {
    let target_path_buf = std::path::Path::new(&target_folder).to_path_buf();
    if let Err(e) = tokio::fs::create_dir_all(&target_path_buf).await {
        return Err(server_error(format!(
            "Failed to create target directory: {}",
            e
        )));
    }

    let res = slskd_download(tracks).await?;
    let download_filenames: Vec<String> = res.iter().map(|d| d.filename.clone()).collect();
    let target_path = target_path_buf;

    tracing::info!("Started monitoring downloads: {:?}", download_filenames);

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        let mut attempts = 0;
        const MAX_ATTEMPTS: usize = 600; // ~20 minutes timeout

        loop {
            interval.tick().await;
            attempts += 1;

            if attempts > MAX_ATTEMPTS {
                info!(
                    "Download monitoring timed out for batch {:?}",
                    download_filenames
                );
                break;
            }

            match SLSKD_CLIENT.get_all_downloads().await {
                Ok(downloads) => {
                    let batch_status: Vec<_> = downloads
                        .iter()
                        .filter(|file| download_filenames.contains(&file.filename))
                        .collect();

                    // If we can't find any of our downloads, they might have been cleared or invalid
                    if batch_status.is_empty() {
                        info!("No active downloads found for batch, assuming completed or lost.");
                        break;
                    }

                    let all_finished = batch_status.iter().all(|d| {
                        d.state.iter().any(|s| {
                            matches!(
                                s,
                                DownloadState::Succeeded
                                    | DownloadState::Completed
                                    | DownloadState::Aborted
                                    | DownloadState::Cancelled
                                    | DownloadState::Errored
                            )
                        })
                    });

                    if all_finished {
                        let successful_downloads: Vec<_> = batch_status
                            .iter()
                            .filter(|d| {
                                d.state.iter().any(|s| {
                                    matches!(s, DownloadState::Succeeded | DownloadState::Completed)
                                })
                            })
                            .collect();

                        if !successful_downloads.is_empty() {
                            info!(
                                "Downloads completed ({} successful). Starting import to {:?}",
                                successful_downloads.len(),
                                target_path
                            );

                            let paths: Vec<String> = successful_downloads
                                .iter()
                                .map(|d| d.filename.clone())
                                .collect();

                            if let Err(e) = beets::import(paths, &target_path).await {
                                info!("Beets import error: {}", e);
                            }
                        } else {
                            info!("Downloads finished but none succeeded. Skipping import.");
                        }
                        break;
                    }
                }
                Err(e) => {
                    info!("Error fetching download status: {}", e);
                }
            }
        }
    });

    Ok(res)
}
