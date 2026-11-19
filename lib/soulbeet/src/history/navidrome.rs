//! Listening history read from the Navidrome library: its songs carry this user's play count,
//! last play date, rating and star.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use shared::recommendation::{
    normalize_for_matching, ArtistRef, KnownMusic, Listen, PlayedArtist, PlayedTrack, TimePeriod,
    TrackRef,
};

use super::HistoryError;
use crate::navidrome::models::NativeSong;
use crate::navidrome::NavidromeClient;
use crate::traits::ListeningHistory;

/// The library's songs with this user's annotations, read once.
pub struct NavidromeHistory {
    songs: Vec<NativeSong>,
    now: DateTime<Utc>,
}

/// Every present song of the library through `all_songs`, with this user's annotations:
/// Navidrome joins this user's `play_count`, `play_date`, `rating` and `starred` onto each song
/// (`withAnnotation`, persistence/sql_annotations.go:62-90 in 0.64.2; :17-41 at fa46aefa).
/// `Incomplete` when the pages held fewer songs than Navidrome counts, since a part of the
/// history would be missing.
pub async fn read(client: &NavidromeClient) -> Result<NavidromeHistory, HistoryError> {
    let page = client.all_songs().await?;
    if (page.songs.len() as u64) < page.total {
        return Err(HistoryError::Incomplete {
            read: page.songs.len(),
            total: page.total,
        });
    }
    Ok(NavidromeHistory::from_songs(page.songs, Utc::now()))
}

fn disliked(song: &NativeSong) -> bool {
    matches!(song.rating, 1 | 2)
}

/// The later of the star and the 4 or 5 rating. `starred_at` also moves on an unstar and
/// `rated_at` on any rating, so each counts only while it still makes a favourite. None
/// sorts below every date: a rating from before Navidrome 0.59.0 has no date.
fn favoured_at(song: &NativeSong) -> Option<DateTime<Utc>> {
    let starred = song.starred_at.filter(|_| song.starred);
    let rated = song.rated_at.filter(|_| song.rating >= 4);
    starred.max(rated)
}

fn track_ref(song: &NativeSong) -> TrackRef {
    TrackRef {
        artist: ArtistRef {
            name: song.artist.clone(),
            mbid: song.mbz_artist_id.clone(),
        },
        title: song.title.clone(),
        mbid: song.mbz_recording_id.clone(),
    }
}

impl NavidromeHistory {
    pub fn from_songs(songs: Vec<NativeSong>, now: DateTime<Utc>) -> Self {
        Self { songs, now }
    }

    /// Played songs the user did not rate 1 or 2. A period keeps those whose last play falls
    /// inside it. Navidrome keeps one last-played date per song (`IncPlayCount` sets
    /// `max(play_date, ts)`, sql_annotations.go), so a period counts all of a song's plays
    /// when its last play falls inside it: an approximation. AllTime keeps every seed song.
    fn seed_songs(&self, period: TimePeriod) -> Vec<&NativeSong> {
        let days = match period {
            TimePeriod::Week => Some(7),
            TimePeriod::Month => Some(30),
            TimePeriod::Quarter => Some(90),
            TimePeriod::HalfYear => Some(182),
            TimePeriod::Year => Some(365),
            TimePeriod::AllTime => None,
        };
        let since = days.map(|d| self.now - Duration::days(d));
        self.songs
            .iter()
            .filter(|s| s.play_count > 0 && !disliked(s))
            .filter(|s| match since {
                Some(since) => s.play_date.is_some_and(|d| d >= since),
                None => true,
            })
            .collect()
    }
}

#[async_trait]
impl ListeningHistory for NavidromeHistory {
    fn name(&self) -> &'static str {
        "Navidrome"
    }

    async fn top_artists(
        &self,
        period: TimePeriod,
        count: usize,
    ) -> Result<Vec<PlayedArtist>, HistoryError> {
        let mut by_key: HashMap<String, usize> = HashMap::new();
        let mut artists: Vec<PlayedArtist> = Vec::new();
        for song in self.seed_songs(period) {
            let key = match &song.mbz_artist_id {
                Some(mbid) => mbid.to_lowercase(),
                None => normalize_for_matching(&song.artist),
            };
            let at = *by_key.entry(key).or_insert_with(|| {
                artists.push(PlayedArtist {
                    artist: ArtistRef {
                        name: song.artist.clone(),
                        mbid: song.mbz_artist_id.clone(),
                    },
                    plays: 0,
                });
                artists.len() - 1
            });
            artists[at].plays += song.play_count;
        }
        artists.sort_by(|a, b| {
            b.plays
                .cmp(&a.plays)
                .then_with(|| a.artist.name.cmp(&b.artist.name))
        });
        artists.truncate(count);
        Ok(artists)
    }

    async fn top_tracks(
        &self,
        period: TimePeriod,
        count: usize,
    ) -> Result<Vec<PlayedTrack>, HistoryError> {
        let mut songs = self.seed_songs(period);
        songs.sort_by(|a, b| {
            b.play_count
                .cmp(&a.play_count)
                .then_with(|| b.play_date.cmp(&a.play_date))
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(songs
            .into_iter()
            .take(count)
            .map(|s| PlayedTrack {
                track: track_ref(s),
                plays: s.play_count,
            })
            .collect())
    }

    async fn listens(&self, count: usize) -> Result<Vec<Listen>, HistoryError> {
        let mut played: Vec<(&NativeSong, DateTime<Utc>)> = self
            .seed_songs(TimePeriod::AllTime)
            .into_iter()
            .filter_map(|s| Some((s, s.play_date?)))
            .collect();
        played.sort_by_key(|p| std::cmp::Reverse(p.1));
        Ok(played
            .into_iter()
            .take(count)
            .map(|(s, date)| Listen {
                artist: s.artist.clone(),
                track: s.title.clone(),
                album: Some(s.album.clone()),
                timestamp: date.timestamp(),
            })
            .collect())
    }

    /// Navidrome lists songs oldest import first; favourites go by when the user favoured them.
    async fn favourites(&self) -> Result<Vec<TrackRef>, HistoryError> {
        let mut songs: Vec<&NativeSong> = self
            .songs
            .iter()
            .filter(|s| !disliked(s) && (s.starred || s.rating >= 4))
            .collect();
        songs.sort_by(|a, b| {
            favoured_at(b)
                .cmp(&favoured_at(a))
                .then_with(|| b.rating.cmp(&a.rating))
                .then_with(|| b.play_count.cmp(&a.play_count))
                .then_with(|| b.play_date.cmp(&a.play_date))
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(songs.into_iter().map(track_ref).collect())
    }

    async fn known(&self) -> Result<KnownMusic, HistoryError> {
        let mut known = KnownMusic::default();
        for song in &self.songs {
            known.add_track(&track_ref(song));
            known.add_artist(&ArtistRef {
                name: song.album_artist.clone(),
                mbid: song.mbz_album_artist_id.clone(),
            });
        }
        Ok(known)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{serve_with_headers, Reply};
    use crate::NavidromeClientBuilder;
    use shared::recommendation::{ArtistRef, TimePeriod};

    fn now() -> DateTime<Utc> {
        "2026-11-19T12:00:00Z".parse().unwrap()
    }

    fn song(
        id: &str,
        artist: &str,
        title: &str,
        plays: u64,
        played: Option<&str>,
        rating: u8,
        starred: bool,
    ) -> NativeSong {
        serde_json::from_value(song_json(id, artist, title, plays, played, rating, starred))
            .unwrap()
    }

    fn song_json(
        id: &str,
        artist: &str,
        title: &str,
        plays: u64,
        played: Option<&str>,
        rating: u8,
        starred: bool,
    ) -> serde_json::Value {
        let mut json = serde_json::json!({
            "id": id, "path": format!("{artist}/{title}.flac"), "title": title,
            "size": 1000, "libraryId": 1, "libraryPath": "/music", "missing": false,
            "updatedAt": "2026-11-01T18:20:03Z", "artist": artist, "album": "Winter Songs",
            "albumArtist": artist, "playCount": plays, "rating": rating, "starred": starred,
        });
        if let Some(date) = played {
            json["playDate"] = date.into();
        }
        json
    }

    /// A song with this user's star and rating dates, as Navidrome 0.59.0 on sends them.
    fn favoured(
        id: &str,
        title: &str,
        starred: bool,
        starred_at: Option<&str>,
        rating: u8,
        rated_at: Option<&str>,
    ) -> NativeSong {
        let mut json = song_json(id, title, title, 0, None, rating, starred);
        if let Some(at) = starred_at {
            json["starredAt"] = at.into();
        }
        if let Some(at) = rated_at {
            json["ratedAt"] = at.into();
        }
        serde_json::from_value(json).unwrap()
    }

    fn library() -> Vec<NativeSong> {
        vec![
            song(
                "1",
                "Static Harbor",
                "Northern Line",
                5,
                Some("2026-11-18T20:00:00Z"),
                0,
                false,
            ),
            song(
                "2",
                "Vela Norte",
                "Foglight",
                9,
                Some("2026-01-02T20:00:00Z"),
                2,
                false,
            ),
            song("3", "Static Harbor", "Undertow", 0, None, 0, false),
            song(
                "4",
                "Vela Norte",
                "Maritime",
                3,
                Some("2026-11-01T20:00:00Z"),
                0,
                false,
            ),
        ]
    }

    fn titles(tracks: &[PlayedTrack]) -> Vec<&str> {
        tracks.iter().map(|t| t.track.title.as_str()).collect()
    }

    #[tokio::test]
    async fn top_tracks_rank_played_songs_and_never_seed_a_disliked_one() {
        let h = NavidromeHistory::from_songs(library(), now());
        let all = h.top_tracks(TimePeriod::AllTime, 10).await.unwrap();
        assert_eq!(titles(&all), ["Northern Line", "Maritime"]);
        let month = h.top_tracks(TimePeriod::Month, 10).await.unwrap();
        assert_eq!(titles(&month), ["Northern Line", "Maritime"]);
        let week = h.top_tracks(TimePeriod::Week, 10).await.unwrap();
        assert_eq!(titles(&week), ["Northern Line"]);
    }

    #[tokio::test]
    async fn top_artists_sum_their_songs_plays() {
        let mut songs = vec![
            song(
                "1",
                "Static Harbor",
                "Northern Line",
                5,
                Some("2026-11-18T20:00:00Z"),
                0,
                false,
            ),
            song(
                "2",
                "Static Harbor",
                "Undertow",
                4,
                Some("2026-11-17T20:00:00Z"),
                0,
                false,
            ),
            song(
                "3",
                "Vela Norte",
                "Maritime",
                3,
                Some("2026-11-01T20:00:00Z"),
                0,
                false,
            ),
        ];
        songs[0].mbz_artist_id = Some("a1".into());
        songs[1].mbz_artist_id = Some("a1".into());
        let h = NavidromeHistory::from_songs(songs, now());
        let artists = h.top_artists(TimePeriod::AllTime, 10).await.unwrap();
        let got: Vec<(&str, u64)> = artists
            .iter()
            .map(|a| (a.artist.name.as_str(), a.plays))
            .collect();
        assert_eq!(got, [("Static Harbor", 9), ("Vela Norte", 3)]);
        assert_eq!(artists[0].artist.mbid.as_deref(), Some("a1"));
    }

    #[tokio::test]
    async fn favourites_are_starred_or_well_rated_songs() {
        let h = NavidromeHistory::from_songs(
            vec![
                song("1", "A", "starred", 0, None, 0, true),
                song("2", "A", "five", 0, None, 5, false),
                song("3", "A", "starred but disliked", 0, None, 2, true),
                song("4", "A", "middling", 0, None, 3, false),
            ],
            now(),
        );
        let favourites = h.favourites().await.unwrap();
        let got: Vec<&str> = favourites.iter().map(|t| t.title.as_str()).collect();
        // Neither has a date, so the higher rating goes first.
        assert_eq!(got, ["five", "starred"]);
    }

    #[tokio::test]
    async fn favourites_put_the_latest_star_or_rating_first() {
        // Navidrome lists songs oldest import first.
        let h = NavidromeHistory::from_songs(
            vec![
                favoured("1", "rated before 0.59", false, None, 5, None),
                favoured(
                    "2",
                    "unstarred lately",
                    false,
                    Some("2026-11-18T10:00:00Z"),
                    4,
                    None,
                ),
                favoured("3", "old star", true, Some("2025-03-01T10:00:00Z"), 0, None),
                favoured("4", "new star", true, Some("2026-11-15T10:00:00Z"), 0, None),
                favoured(
                    "5",
                    "rated 5 today",
                    false,
                    None,
                    5,
                    Some("2026-11-19T09:00:00Z"),
                ),
                favoured(
                    "6",
                    "rated 3 today",
                    true,
                    Some("2025-06-01T10:00:00Z"),
                    3,
                    Some("2026-11-19T11:00:00Z"),
                ),
            ],
            now(),
        );
        let favourites = h.favourites().await.unwrap();
        let got: Vec<&str> = favourites.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(
            got,
            [
                "rated 5 today",
                "new star",
                "rated 3 today",
                "old star",
                "rated before 0.59",
                "unstarred lately"
            ]
        );
    }

    #[tokio::test]
    async fn the_whole_library_is_known_with_album_artists() {
        let mut songs = library();
        songs[2].album_artist = "Marrow Choir".into();
        songs[2].mbz_album_artist_id = Some("B2".into());
        let known = NavidromeHistory::from_songs(songs, now())
            .known()
            .await
            .unwrap();
        let undertow = TrackRef {
            artist: ArtistRef {
                name: "Static Harbor".into(),
                mbid: None,
            },
            title: "Undertow".into(),
            mbid: None,
        };
        assert!(known.knows_track(&undertow));
        assert!(known.knows_artist(&ArtistRef {
            name: "Marrow Choir".into(),
            mbid: None
        }));
        assert!(known.knows_artist(&ArtistRef {
            name: "other".into(),
            mbid: Some("b2".into())
        }));
    }

    #[tokio::test]
    async fn listens_are_one_per_played_song_newest_first() {
        let h = NavidromeHistory::from_songs(library(), now());
        let listens = h.listens(10).await.unwrap();
        let got: Vec<&str> = listens.iter().map(|l| l.track.as_str()).collect();
        assert_eq!(got, ["Northern Line", "Maritime"]);
        assert_eq!(listens[0].album.as_deref(), Some("Winter Songs"));
        assert_eq!(
            listens[0].timestamp,
            "2026-11-18T20:00:00Z"
                .parse::<DateTime<Utc>>()
                .unwrap()
                .timestamp()
        );
    }

    #[tokio::test]
    async fn a_library_read_short_of_its_count_is_incomplete() {
        let login = r#"{"id":"8uY3tR6eW9qA2sD5fG8hJ1","name":"Alice","username":"alice","isAdmin":false,"token":"eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.e30.c2ln"}"#;
        let page = serde_json::to_string(&[serde_json::json!({
            "id": "1", "path": "a.flac", "title": "t", "size": 1, "libraryId": 1,
            "libraryPath": "/music", "missing": false, "updatedAt": "2026-11-01T18:20:03Z",
            "artist": "a", "album": "b", "albumArtist": "a",
        })])
        .unwrap();
        let (url, _) = serve_with_headers(vec![
            (
                Reply {
                    status: 200,
                    body: login.into(),
                },
                vec![],
            ),
            (
                Reply {
                    status: 200,
                    body: page,
                },
                vec![("X-Total-Count", "2".into())],
            ),
        ]);
        let client = NavidromeClientBuilder::new()
            .base_url(&url)
            .username("alice")
            .password("secret-password")
            .build()
            .unwrap();
        let err = read(&client).await.err().unwrap();
        assert!(
            matches!(err, HistoryError::Incomplete { read: 1, total: 2 }),
            "{err:?}"
        );
    }
}
