use chrono::Duration;
use dioxus::fullstack::{CborEncoding, Streaming};
use dioxus::logger::tracing::info;
use dioxus::prelude::*;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use shared::{
    download::DownloadQuery,
    musicbrainz::{AlbumWithTracks, SearchResult},
    slskd::AlbumResult,
};

#[cfg(feature = "server")]
use soulbeet::musicbrainz;

use super::server_error;

#[cfg(feature = "server")]
use crate::AuthSession;

#[cfg(feature = "server")]
use crate::globals::SLSKD_CLIENT;

static SLSKD_MAX_SEARCH_DURATION: i64 = 120; // seconds

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchQuery {
    pub artist: Option<String>,
    pub query: String,
}

#[post("/api/musicbrainz/search/album", _: AuthSession)]
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

#[post("/api/musicbrainz/search/track", _: AuthSession)]
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

#[get("/api/musicbrainz/album/:id", _: AuthSession)]
pub async fn find_album(id: String) -> Result<AlbumWithTracks, ServerFnError> {
    musicbrainz::find_album(&id).await.map_err(server_error)
}

#[post("/api/slskd/search", _: AuthSession)]
pub async fn search_downloads(
    data: DownloadQuery,
) -> Result<Streaming<Vec<AlbumResult>, CborEncoding>, ServerFnError> {
    let artist = data.album.artist;
    let album = data.album.title;
    let tracks = data.tracks;

    let stream = SLSKD_CLIENT
        .search(
            artist,
            album,
            tracks,
            Duration::seconds(SLSKD_MAX_SEARCH_DURATION),
        )
        .await
        .map_err(server_error)?;

    let mut stream = Box::pin(stream);

    Ok(Streaming::spawn(|tx| async move {
        while let Some(result) = stream.next().await {
            match result {
                Ok(albums) => {
                    // Trial and error to find a good chunk size, too large make a Decoding error in the client
                    // Not sure why yet.
                    for chunk in albums.chunks(1) {
                        let batch = chunk.to_vec();
                        if let Err(err) = tx.unbounded_send(batch) {
                            info!("Client disconnected, stopping stream: {:?}", err);
                            break;
                        }
                    }
                }
                Err(e) => {
                    info!("Error in search stream: {:?}", e);
                    break;
                }
            }
        }
    }))
}
