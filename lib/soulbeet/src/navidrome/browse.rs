//! Navidrome client methods for library scanning with options, artist lookup, and browsing
//! songs/albums with genre and timestamp metadata.

use tracing::info;

use super::client::{found, native_failure, parse, NavidromeClient, SONG_PAGE};
use super::error::{NavidromeError, Result};
use super::models::{
    ArtistBody, BrowseAlbumList2Body, NativeBrowseSong, PingBody, SubsonicArtistWithAlbums,
    SubsonicBrowseAlbum,
};

/// A page of `list_browse_songs`: its songs (carrying `genre`, `genres` and `created_at`), and
/// how many matching songs Navidrome counted in `X-Total-Count`.
#[derive(Debug)]
pub struct BrowseSongPage {
    pub songs: Vec<NativeBrowseSong>,
    pub total: u64,
}

impl NavidromeClient {
    /// `startScan` as a form post, passing `fullScan=true` when `full_scan` is set
    /// (`server/subsonic/library_scanning.go:48`).
    pub async fn start_scan_options(&self, full_scan: bool) -> Result<()> {
        let params: &[(&str, &str)] = if full_scan {
            &[("fullScan", "true")]
        } else {
            &[]
        };
        let _: PingBody = self.post_form("startScan", params).await?;
        Ok(())
    }

    /// `getArtist` with its albums, as this user sees the artist; `Ok(None)` on `DATA_NOT_FOUND`
    /// (`server/subsonic/browsing.go:165-186`).
    pub async fn get_artist(&self, id: &str) -> Result<Option<SubsonicArtistWithAlbums>> {
        let Some(body) = found(self.get::<ArtistBody>("getArtist", &[("id", id)]).await)? else {
            return Ok(None);
        };
        let artist = body
            .artist
            .ok_or_else(|| NavidromeError::Parse("getArtist answered without an artist".into()))?;
        if artist.artist.id != id {
            return Err(NavidromeError::Parse(format!(
                "Navidrome answered another artist for {id}"
            )));
        }
        Ok(Some(artist))
    }

    /// Songs from `GET /api/song` rows `start` to `end` (exclusive) with `genre`, `genres` and
    /// `createdAt`. When `include_missing` is false, sends `missing=false`; when true, omits the
    /// `missing` filter so both present and missing songs are returned.
    pub async fn list_browse_songs(
        &self,
        start: u32,
        end: u32,
        include_missing: bool,
    ) -> Result<BrowseSongPage> {
        let missing = (!include_missing).then_some(false);
        self.list_browse_songs_filtered(start, end, missing).await
    }

    /// Every song read `SONG_PAGE` at a time through `list_browse_songs` until a short page,
    /// deduplicated by song id in first-seen order.
    pub async fn all_browse_songs(&self, include_missing: bool) -> Result<BrowseSongPage> {
        let missing = (!include_missing).then_some(false);
        self.all_browse_songs_filtered(missing).await
    }

    /// `GET /api/song` rows `start` to `end` (exclusive), optionally filtered by `missing`
    /// (`Some(false)` -> `missing=false`, `Some(true)` -> `missing=true`, `None` -> omitted).
    pub async fn list_browse_songs_filtered(
        &self,
        start: u32,
        end: u32,
        missing: Option<bool>,
    ) -> Result<BrowseSongPage> {
        let url = self.base_url.join("api/song")?;
        let mut query = vec![
            ("_start", start.to_string()),
            ("_end", end.to_string()),
            ("_sort", "recently_added".to_string()),
            ("_order", "ASC".to_string()),
        ];
        if let Some(m) = missing {
            query.push(("missing", if m { "true" } else { "false" }.to_string()));
        }
        let resp = self
            .native_request(self.client.get(url).query(&query))
            .await?;
        if !resp.status().is_success() {
            return Err(native_failure("List songs", resp).await);
        }
        let total = resp
            .headers()
            .get("x-total-count")
            .and_then(|count| count.to_str().ok()?.parse().ok())
            .ok_or_else(|| {
                NavidromeError::Parse("Navidrome listed songs without a valid X-Total-Count".into())
            })?;
        let songs = resp.json().await.map_err(parse)?;
        Ok(BrowseSongPage { songs, total })
    }

    /// Every song matching `missing`, paged `SONG_PAGE` at a time until a short page,
    /// deduplicated by song id in first-seen order.
    pub async fn all_browse_songs_filtered(
        &self,
        missing: Option<bool>,
    ) -> Result<BrowseSongPage> {
        let mut songs = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut start = 0;
        loop {
            let page = self
                .list_browse_songs_filtered(start, start + SONG_PAGE, missing)
                .await?;
            let short = page.songs.len() < SONG_PAGE as usize;
            for song in page.songs {
                if seen.insert(song.song.id.clone()) {
                    songs.push(song);
                }
            }
            if short {
                return Ok(BrowseSongPage {
                    songs,
                    total: page.total,
                });
            }
            start += SONG_PAGE;
        }
    }

    /// Every album from `getAlbumList2` (`alphabeticalByName`, paged 500 at a time), carrying
    /// `created`, `played` and `genres` alongside `SubsonicAlbum`.
    pub async fn all_browse_albums(&self) -> Result<Vec<SubsonicBrowseAlbum>> {
        let mut all_albums = Vec::new();
        let mut offset = 0u32;
        let page_size = 500;

        loop {
            let offset_str = offset.to_string();
            let size_str = page_size.to_string();
            let body: BrowseAlbumList2Body = self
                .get(
                    "getAlbumList2",
                    &[
                        ("type", "alphabeticalByName"),
                        ("size", &size_str),
                        ("offset", &offset_str),
                    ],
                )
                .await?;

            let albums = body.album_list.map(|al| al.album).unwrap_or_default();
            let count = albums.len();
            all_albums.extend(albums);

            if count < page_size as usize {
                break;
            }
            offset += page_size;
        }

        info!("Fetched {} browse albums from Navidrome", all_albums.len());
        Ok(all_albums)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::navidrome::NavidromeClientBuilder;
    use crate::test_http::{serve, serve_with_headers, Reply};

    fn client(base_url: &str) -> NavidromeClient {
        NavidromeClientBuilder::new()
            .base_url(base_url)
            .username("alice")
            .password("secret-password")
            .build()
            .unwrap()
    }

    const OK: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#;

    #[tokio::test]
    async fn start_scan_options_sends_full_scan_only_when_requested() {
        let (url, seen) = serve(vec![
            Reply {
                status: 200,
                body: OK.into(),
            },
            Reply {
                status: 200,
                body: OK.into(),
            },
        ]);
        let navi = client(&url);
        navi.start_scan_options(false).await.unwrap();
        navi.start_scan_options(true).await.unwrap();

        let requests = seen.lock().unwrap();
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].target, "/rest/startScan");
        assert!(!requests[0].body.contains("fullScan"));

        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].target, "/rest/startScan");
        assert!(
            requests[1].body.split('&').any(|p| p == "fullScan=true"),
            "{}",
            requests[1].body
        );
    }

    #[tokio::test]
    async fn get_artist_reads_artist_with_albums_and_maps_not_found_to_none() {
        let artist_ok = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","artist":{"id":"ar1","name":"Marrow Choir","albumCount":1,"starred":"2026-11-02T18:04:51Z","userRating":5,"musicBrainzId":"4d5e6f7a-8b9c-4d0e-9f1a-2b3c4d5e6f7a","album":[{"id":"al1","name":"Winter Songs","artist":"Marrow Choir","songCount":12,"duration":2841,"year":2019,"genre":"Choral","userRating":4,"playCount":9}]}}}"#;
        let not_found = r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":70,"message":"Artist not found"}}}"#;
        let wrong_id = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","artist":{"id":"ar2","name":"Other"}}}"#;
        let (url, seen) = serve(vec![
            Reply {
                status: 200,
                body: artist_ok.into(),
            },
            Reply {
                status: 200,
                body: not_found.into(),
            },
            Reply {
                status: 200,
                body: wrong_id.into(),
            },
        ]);
        let navi = client(&url);

        let found = navi.get_artist("ar1").await.unwrap().unwrap();
        assert_eq!(found.artist.id, "ar1");
        assert_eq!(found.artist.name, "Marrow Choir");
        assert_eq!(found.artist.album_count, Some(1));
        assert_eq!(found.artist.user_rating, Some(5));
        assert_eq!(found.album.len(), 1);
        assert_eq!(found.album[0].id, "al1");
        assert_eq!(found.album[0].display_name(), "Winter Songs");

        assert!(navi.get_artist("missing").await.unwrap().is_none());

        let err = navi.get_artist("ar1").await.unwrap_err();
        assert!(matches!(err, NavidromeError::Parse(_)));
        assert!(seen.lock().unwrap()[0].target.contains("id=ar1"));
    }

    #[tokio::test]
    async fn list_browse_songs_handles_genre_genres_created_at_and_missing_filter() {
        let songs = r#"[{"id":"s1","libraryId":1,"libraryPath":"/music","path":"a.flac","title":"T1","album":"A1","artist":"Art1","albumArtist":"Art1","size":10,"missing":false,"updatedAt":"2026-11-01T10:00:00Z","createdAt":"2026-10-15T08:00:00Z","genre":"Ambient","genres":[{"id":"g1","name":"Ambient"},{"id":"g2","name":"Drone"}]}]"#;
        let (url, seen) = serve_with_headers(vec![
            (
                Reply {
                    status: 200,
                    body: r#"{"token":"jwt"}"#.into(),
                },
                vec![],
            ),
            (
                Reply {
                    status: 200,
                    body: songs.into(),
                },
                vec![("X-Total-Count", "1".to_string())],
            ),
            (
                Reply {
                    status: 200,
                    body: songs.into(),
                },
                vec![("X-Total-Count", "1".to_string())],
            ),
            (
                Reply {
                    status: 200,
                    body: songs.into(),
                },
                vec![("X-Total-Count", "1".to_string())],
            ),
        ]);
        let navi = client(&url);

        let present = navi.all_browse_songs(false).await.unwrap();
        assert_eq!(present.total, 1);
        assert_eq!(present.songs.len(), 1);
        assert_eq!(present.songs[0].genre, "Ambient");
        assert_eq!(present.songs[0].genres.len(), 2);
        assert_eq!(present.songs[0].genres[1].name, "Drone");
        assert_eq!(
            present.songs[0].created_at,
            Some("2026-10-15T08:00:00Z".parse().unwrap())
        );

        let _all = navi.all_browse_songs(true).await.unwrap();
        let _only_missing = navi.all_browse_songs_filtered(Some(true)).await.unwrap();

        let requests = seen.lock().unwrap();
        assert!(requests[1].target.contains("missing=false"));
        assert!(!requests[2].target.contains("missing="));
        assert!(requests[3].target.contains("missing=true"));
    }
}
