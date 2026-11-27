//! Deezer public playlist resolver built on `DeezerClient`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use shared::playlist_import::{ExternalPlaylist, ExternalTrack, PlaylistPlatform};

use crate::deezer::DeezerClient;
use crate::knowledge::{CacheError, KnowledgeError};
use crate::polite::{Pace, RemoteError};
use crate::traits::KnowledgeCache;

use super::{PlaylistImportError, MAX_PLAYLIST_TRACKS};

const DEEZER_API_BASE: &str = "https://api.deezer.com";
const TRUNCATED_500_NOTE: &str = "Showing the first 500 tracks of this playlist.";

struct NoopCache;

#[async_trait]
impl KnowledgeCache for NoopCache {
    async fn get(&self, _key: &str) -> Result<Option<String>, CacheError> {
        Ok(None)
    }

    async fn put(&self, _key: &str, _value: &str, _ttl: Duration) -> Result<(), CacheError> {
        Ok(())
    }
}

/// Fetches a public Deezer playlist by numeric ID.
pub async fn fetch_playlist(
    id: &str,
    source_url: &str,
) -> Result<ExternalPlaylist, PlaylistImportError> {
    fetch_playlist_from(DEEZER_API_BASE, id, source_url, Pace::DEEZER).await
}

pub(crate) async fn fetch_playlist_from(
    base_url: &str,
    id: &str,
    source_url: &str,
    pace: Pace,
) -> Result<ExternalPlaylist, PlaylistImportError> {
    let client = DeezerClient::new(base_url, pace, Arc::new(NoopCache))
        .map_err(|e| PlaylistImportError::Upstream(e.to_string()))?;

    let playlist = match client.playlist(id, MAX_PLAYLIST_TRACKS).await {
        Ok(Some(pl)) => pl,
        Ok(None) => {
            return Err(PlaylistImportError::NotFound(
                "Deezer playlist was not found or is private.".into(),
            ));
        }
        Err(KnowledgeError::Remote(RemoteError::Status {
            status: 404 | 403, ..
        })) => {
            return Err(PlaylistImportError::NotFound(
                "Deezer playlist was not found or is private.".into(),
            ));
        }
        Err(KnowledgeError::Deezer { code: 800, .. }) => {
            return Err(PlaylistImportError::NotFound(
                "Deezer playlist was not found or is private.".into(),
            ));
        }
        Err(e) => return Err(PlaylistImportError::Upstream(e.to_string())),
    };

    if playlist.tracks.is_empty() {
        return Err(PlaylistImportError::EmptyPlaylist);
    }

    let truncated_note = (playlist.tracks.len() >= MAX_PLAYLIST_TRACKS
        && playlist.nb_tracks as usize > MAX_PLAYLIST_TRACKS)
        .then(|| TRUNCATED_500_NOTE.to_string());

    let tracks = playlist
        .tracks
        .into_iter()
        .map(|t| ExternalTrack {
            artist: t.artist,
            title: t.title,
            album: t.album,
            duration_s: t.duration_s,
            recording_mbid: None,
        })
        .collect();

    Ok(ExternalPlaylist {
        platform: PlaylistPlatform::Deezer,
        source_url: source_url.to_string(),
        title: if playlist.title.is_empty() {
            "Deezer Playlist".to_string()
        } else {
            playlist.title
        },
        description: playlist.description,
        tracks,
        truncated_note,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{serve, Reply};

    #[tokio::test]
    async fn resolves_and_paginates_deezer_playlist_and_retries_quota() {
        let initial = serde_json::json!({
            "id": 908622995u64,
            "title": "French Touch Essentials",
            "description": "Classic Parisian club cuts",
            "nb_tracks": 3,
            "tracks": {
                "data": [
                    {
                        "title": "Around the World",
                        "duration": 429,
                        "artist": { "name": "Daft Punk" },
                        "album": { "title": "Homework" }
                    },
                    {
                        "title": "Music Sounds Better With You",
                        "duration": 404,
                        "artist": { "name": "Stardust" },
                        "album": { "title": "Music Sounds Better With You" }
                    }
                ],
                "next": "https://api.deezer.com/playlist/908622995/tracks?index=2"
            }
        });
        let quota = r#"{"error":{"type":"Exception","message":"Quota limit exceeded","code":4}}"#;
        let page2 = serde_json::json!({
            "data": [
                {
                    "title": "Lady (Hear Me Tonight)",
                    "duration": 307,
                    "artist": { "name": "Modjo" },
                    "album": { "title": "Modjo" }
                }
            ]
        });

        let (base, seen) = serve(vec![
            Reply {
                status: 200,
                body: initial.to_string(),
            },
            Reply {
                status: 200,
                body: quota.into(),
            },
            Reply {
                status: 200,
                body: page2.to_string(),
            },
        ]);

        let pl = fetch_playlist_from(
            &base,
            "908622995",
            "https://www.deezer.com/playlist/908622995",
            Pace::immediate(),
        )
        .await
        .unwrap();

        assert_eq!(pl.platform, PlaylistPlatform::Deezer);
        assert_eq!(pl.title, "French Touch Essentials");
        assert_eq!(pl.description.as_deref(), Some("Classic Parisian club cuts"));
        assert_eq!(pl.tracks.len(), 3);
        assert_eq!(
            pl.tracks[0],
            ExternalTrack {
                artist: "Daft Punk".into(),
                title: "Around the World".into(),
                album: Some("Homework".into()),
                duration_s: Some(429),
                recording_mbid: None,
            }
        );
        assert_eq!(pl.tracks[2].artist, "Modjo");

        let reqs = seen.lock().unwrap();
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs[0].target, "/playlist/908622995");
        assert_eq!(reqs[1].target, "/playlist/908622995/tracks?index=2&limit=100");
        assert_eq!(reqs[2].target, "/playlist/908622995/tracks?index=2&limit=100");
    }

    #[tokio::test]
    async fn deezer_code_800_maps_to_not_found() {
        let (base, _) = serve(vec![Reply {
            status: 200,
            body: r#"{"error":{"type":"DataException","message":"no data","code":800}}"#.into(),
        }]);
        let err = fetch_playlist_from(
            &base,
            "999999999",
            "https://www.deezer.com/playlist/999999999",
            Pace::immediate(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PlaylistImportError::NotFound(_)));
    }
}
