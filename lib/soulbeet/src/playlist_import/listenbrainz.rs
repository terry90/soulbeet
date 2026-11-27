//! ListenBrainz public JSPF playlist resolver.

use shared::playlist_import::{ExternalPlaylist, ExternalTrack, PlaylistPlatform};

use crate::listenbrainz::client::ListenBrainzClient;
use crate::polite::{Pace, RemoteError};

use super::{PlaylistImportError, MAX_PLAYLIST_TRACKS};

const LISTENBRAINZ_API_BASE: &str = "https://api.listenbrainz.org";
const TRUNCATED_500_NOTE: &str = "Showing the first 500 tracks of this playlist.";

/// Fetches a ListenBrainz JSPF playlist by MBID.
pub async fn fetch_playlist(
    mbid: &str,
    source_url: &str,
) -> Result<ExternalPlaylist, PlaylistImportError> {
    fetch_playlist_from_inner(LISTENBRAINZ_API_BASE, mbid, source_url, Pace::METABRAINZ, true).await
}

#[cfg(test)]
pub(crate) async fn fetch_playlist_from(
    base_url: &str,
    mbid: &str,
    source_url: &str,
    pace: Pace,
) -> Result<ExternalPlaylist, PlaylistImportError> {
    fetch_playlist_from_inner(base_url, mbid, source_url, pace, false).await
}

async fn fetch_playlist_from_inner(
    base_url: &str,
    mbid: &str,
    source_url: &str,
    pace: Pace,
    enforce_lb_host: bool,
) -> Result<ExternalPlaylist, PlaylistImportError> {
    let client = ListenBrainzClient::without_redirects(base_url, None, pace)
        .map_err(|e| PlaylistImportError::Upstream(e.to_string()))?;

    let playlist = match client.playlist_full_bounded(mbid, enforce_lb_host).await {
        Ok(Some(pl)) => pl,
        Ok(None) => {
            return Err(PlaylistImportError::NotFound(
                "ListenBrainz playlist was not found or is private.".into(),
            ));
        }
        Err(RemoteError::Status {
            status: 404 | 403, ..
        }) => {
            return Err(PlaylistImportError::NotFound(
                "ListenBrainz playlist was not found or is private.".into(),
            ));
        }
        Err(e) => return Err(PlaylistImportError::Upstream(e.to_string())),
    };

    let total = playlist.tracks.len();
    let tracks: Vec<ExternalTrack> = playlist
        .tracks
        .into_iter()
        .take(MAX_PLAYLIST_TRACKS)
        .map(|t| ExternalTrack {
            artist: t.creator,
            title: t.title,
            album: t.album,
            duration_s: t.duration_s,
            recording_mbid: t.recording_mbid,
        })
        .collect();

    if tracks.is_empty() {
        return Err(PlaylistImportError::EmptyPlaylist);
    }

    let truncated_note = (total > MAX_PLAYLIST_TRACKS).then(|| TRUNCATED_500_NOTE.to_string());

    Ok(ExternalPlaylist {
        platform: PlaylistPlatform::ListenBrainz,
        source_url: source_url.to_string(),
        title: playlist
            .title
            .unwrap_or_else(|| "ListenBrainz Playlist".to_string()),
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
    async fn resolves_listenbrainz_playlist_preserving_mbids_album_and_duration() {
        let fixture = r#"{
            "playlist": {
                "title": "Ambient Journeys",
                "annotation": "Long-form space ambient",
                "track": [
                    {
                        "title": "Fauna",
                        "creator": "Carbon Based Lifeforms",
                        "album": "World of Sleepers",
                        "duration": 312000,
                        "identifier": ["https://musicbrainz.org/recording/22c7fa7f-2404-4103-8267-6a98b4b1f8ee"]
                    },
                    {
                        "title": "Untitled Without Creator",
                        "identifier": []
                    },
                    {
                        "title": "Photosynthesis",
                        "creator": "Carbon Based Lifeforms",
                        "duration": 350000,
                        "identifier": ["https://musicbrainz.org/recording/7f3593da-7abc-4ae1-943e-cda2e4a00771"]
                    }
                ]
            }
        }"#;
        let (base, seen) = serve(vec![Reply {
            status: 200,
            body: fixture.into(),
        }]);

        let mbid = "48f2eb97-33e7-496c-8da1-25ead7280a81";
        let pl = fetch_playlist_from(
            &base,
            mbid,
            &format!("https://listenbrainz.org/playlist/{mbid}"),
            Pace::immediate(),
        )
        .await
        .unwrap();

        assert_eq!(seen.lock().unwrap()[0].target, format!("/1/playlist/{mbid}"));
        assert_eq!(pl.platform, PlaylistPlatform::ListenBrainz);
        assert_eq!(pl.title, "Ambient Journeys");
        assert_eq!(pl.description.as_deref(), Some("Long-form space ambient"));
        assert_eq!(pl.tracks.len(), 2);
        assert_eq!(
            pl.tracks[0],
            ExternalTrack {
                artist: "Carbon Based Lifeforms".into(),
                title: "Fauna".into(),
                album: Some("World of Sleepers".into()),
                duration_s: Some(312),
                recording_mbid: Some("22c7fa7f-2404-4103-8267-6a98b4b1f8ee".into()),
            }
        );
        assert_eq!(pl.tracks[1].duration_s, Some(350));
        assert_eq!(pl.tracks[1].album, None);
    }

    #[tokio::test]
    async fn handles_404_as_not_found() {
        let (base, _) = serve(vec![Reply {
            status: 404,
            body: r#"{"code":404,"error":"Cannot find playlist"}"#.into(),
        }]);
        let err = fetch_playlist_from(
            &base,
            "48f2eb97-33e7-496c-8da1-25ead7280a81",
            "https://listenbrainz.org/playlist/48f2eb97-33e7-496c-8da1-25ead7280a81",
            Pace::immediate(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PlaylistImportError::NotFound(_)));
    }

    #[tokio::test]
    async fn ignores_non_recording_or_non_musicbrainz_identifiers_and_blocks_redirects() {
        use crate::test_http::serve_with_headers;

        let fixture = r#"{
            "playlist": {
                "title": "Mixed Identifiers",
                "track": [
                    {
                        "title": "Spotify Track",
                        "creator": "Some Artist",
                        "identifier": ["https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT"]
                    },
                    {
                        "title": "Release URL Not Recording",
                        "creator": "Some Artist",
                        "identifier": ["https://musicbrainz.org/release/22c7fa7f-2404-4103-8267-6a98b4b1f8ee"]
                    },
                    {
                        "title": "Second Identifier Is Valid",
                        "creator": "Some Artist",
                        "identifier": [
                            "https://open.spotify.com/track/notambid",
                            "https://musicbrainz.org/recording/22c7fa7f-2404-4103-8267-6a98b4b1f8ee"
                        ]
                    }
                ]
            }
        }"#;
        let mbid = "48f2eb97-33e7-496c-8da1-25ead7280a81";
        let (base, _) = serve(vec![Reply {
            status: 200,
            body: fixture.into(),
        }]);
        let pl = fetch_playlist_from(
            &base,
            mbid,
            &format!("https://listenbrainz.org/playlist/{mbid}"),
            Pace::immediate(),
        )
        .await
        .unwrap();
        assert_eq!(pl.tracks.len(), 3);
        assert_eq!(pl.tracks[0].recording_mbid, None);
        assert_eq!(pl.tracks[1].recording_mbid, None);
        assert_eq!(
            pl.tracks[2].recording_mbid.as_deref(),
            Some("22c7fa7f-2404-4103-8267-6a98b4b1f8ee")
        );

        let (redir_base, _) = serve_with_headers(vec![(
            Reply {
                status: 302,
                body: String::new(),
            },
            vec![("Location", "http://127.0.0.1:9999/secret".into())],
        )]);
        let err = fetch_playlist_from(
            &redir_base,
            mbid,
            &format!("https://listenbrainz.org/playlist/{mbid}"),
            Pace::immediate(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PlaylistImportError::Upstream(_)), "{err:?}");
    }
}
