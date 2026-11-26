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
use crate::navidrome::models::{NativeArtist, NativeSong, Participant};
use crate::navidrome::NavidromeClient;
use crate::traits::ListeningHistory;

/// The library's songs with this user's annotations, read once.
pub struct NavidromeHistory {
    songs: Vec<NativeSong>,
    /// Songs excluded from core taste profile building (e.g. seasonal packs) but still owned
    /// in the library so `known()` keeps them out of future recommendations.
    excluded_from_taste: Vec<NativeSong>,
    /// The MusicBrainz id of each artist row that has one, by Navidrome artist id.
    artist_mbids: HashMap<String, String>,
    now: DateTime<Utc>,
}

/// Every present song of the library through `all_songs`, with this user's annotations:
/// Navidrome joins this user's `play_count`, `play_date`, `rating` and `starred` onto each song
/// (`withAnnotation`, persistence/sql_annotations.go:62-90 in 0.64.2; :17-41 at fa46aefa).
/// `Incomplete` when the pages held fewer songs than Navidrome counts, since a part of the
/// history would be missing. The artist rows follow, as they hold the artist MBIDs from
/// Navidrome 0.55.0 on.
pub async fn read(client: &NavidromeClient) -> Result<NavidromeHistory, HistoryError> {
    let page = client.all_songs().await?;
    if (page.songs.len() as u64) < page.total {
        return Err(HistoryError::Incomplete {
            read: page.songs.len(),
            total: page.total,
        });
    }
    let artists = client.list_artists().await?;
    Ok(NavidromeHistory::from_songs(
        page.songs,
        artists,
        Utc::now(),
    ))
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

impl NavidromeHistory {
    pub fn from_songs(
        songs: Vec<NativeSong>,
        artists: Vec<NativeArtist>,
        now: DateTime<Utc>,
    ) -> Self {
        let artist_mbids = artists
            .into_iter()
            .filter_map(|a| Some((a.id, a.mbz_artist_id?)))
            .collect();
        Self {
            songs,
            excluded_from_taste: Vec::new(),
            artist_mbids,
            now,
        }
    }

    /// Excludes songs matching `is_excluded` (such as seasonal discovery tracks with
    /// `exclude_from_core_taste = true`) from core taste seed queries (`top_artists`,
    /// `top_tracks`, `listens`, `favourites`), while retaining them in `known()` so owned
    /// seasonal files are still recognized as already in the library.
    pub fn exclude_from_core_taste<F>(mut self, is_excluded: F) -> Self
    where
        F: Fn(&NativeSong) -> bool,
    {
        for song in &self.songs {
            if is_excluded(song) {
                self.excluded_from_taste.push(song.clone());
            }
        }
        self.songs = crate::engine::contextual::filter_core_taste_tracks(self.songs, is_excluded);
        self
    }

    fn mbid_of(&self, participant: &Participant) -> Option<String> {
        self.artist_mbids.get(&participant.id).cloned()
    }

    /// The song's main artist, its first `artist` participant from Navidrome 0.55.0 on. It is
    /// named as its artist row, so a collaboration counts under the same name as a solo song.
    /// The MBID is the song's own where the scanner before 0.55.0 filled it, else the row's.
    /// Songs scanned before 0.55.0 have no participants, only the display artist.
    fn artist_of(&self, song: &NativeSong) -> ArtistRef {
        let main = song.participants.artist.first();
        ArtistRef {
            name: main.map_or(&song.artist, |m| &m.name).clone(),
            mbid: song.mbz_artist_id.clone().or_else(|| self.mbid_of(main?)),
        }
    }

    fn track_ref(&self, song: &NativeSong) -> TrackRef {
        TrackRef {
            artist: self.artist_of(song),
            title: song.title.clone(),
            mbid: song.mbz_recording_id.clone(),
        }
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
            let artist = self.artist_of(song);
            let key = match &artist.mbid {
                Some(mbid) => mbid.to_lowercase(),
                None => normalize_for_matching(&artist.name),
            };
            let at = *by_key.entry(key).or_insert_with(|| {
                artists.push(PlayedArtist { artist, plays: 0 });
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
                track: self.track_ref(s),
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
                artist: self.artist_of(s).name,
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
        Ok(songs.into_iter().map(|s| self.track_ref(s)).collect())
    }

    async fn known(&self) -> Result<KnownMusic, HistoryError> {
        let mut known = KnownMusic::default();
        for song in self.songs.iter().chain(&self.excluded_from_taste) {
            // A recommended track carries its credited artist string, which names every artist.
            let mut credited = self.track_ref(song);
            credited.artist.name = song.artist.clone();
            known.add_track(&credited);
            known.add_artist(&ArtistRef {
                name: song.album_artist.clone(),
                mbid: song.mbz_album_artist_id.clone(),
            });
            // Every credited artist, so a recommendation under any of their names or MBIDs is known.
            let credited = song.participants.artist.iter();
            for participant in credited.chain(&song.participants.albumartist) {
                known.add_artist(&ArtistRef {
                    name: participant.name.clone(),
                    mbid: self.mbid_of(participant),
                });
            }
        }
        Ok(known)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::engine::build_profile;
    use crate::engine::test_support::FakeKnowledge;
    use crate::test_http::{serve_with_headers, Reply};
    use crate::NavidromeClientBuilder;
    use shared::recommendation::{ArtistRef, TimePeriod};

    fn now() -> DateTime<Utc> {
        "2026-11-19T12:00:00Z".parse().unwrap()
    }

    fn history(songs: Vec<NativeSong>) -> NavidromeHistory {
        NavidromeHistory::from_songs(songs, Vec::new(), now())
    }

    const SH: &str = "1c2d3e4f-5a6b-4c7d-8e9f-0a1b2c3d4e5f";
    const VN: &str = "2e3f4a5b-6c7d-4e8f-9a0b-1c2d3e4f5a6b";
    const MC: &str = "3f4a5b6c-7d8e-4f9a-0b1c-2d3e4f5a6b7c";

    /// The artist rows of Static Harbor, Vela Norte and Marrow Choir, with their MBIDs.
    fn artist_rows() -> Vec<NativeArtist> {
        serde_json::from_value(serde_json::json!([
            {"id": "ar-sh", "name": "Static Harbor", "mbzArtistId": SH, "missing": false},
            {"id": "ar-vn", "name": "Vela Norte", "mbzArtistId": VN, "missing": false},
            {"id": "ar-mc", "name": "Marrow Choir", "mbzArtistId": MC, "missing": false},
            {"id": "ar-none", "name": "Untagged", "missing": false},
        ]))
        .unwrap()
    }

    /// A played song as Navidrome 0.55.0 on sends it: no artist MBIDs, the artists by id.
    fn credited_json(
        id: &str,
        title: &str,
        artists: &[(&str, &str)],
        album_artist: (&str, &str),
    ) -> serde_json::Value {
        let display = artists
            .iter()
            .map(|a| a.1)
            .collect::<Vec<_>>()
            .join(" feat. ");
        let mut json = song_json(
            id,
            &display,
            title,
            3,
            Some("2026-11-18T20:00:00Z"),
            0,
            false,
        );
        let list = |credits: &[(&str, &str)]| -> serde_json::Value {
            credits
                .iter()
                .map(|(id, name)| serde_json::json!({"id": id, "name": name, "missing": false}))
                .collect()
        };
        json["albumArtist"] = album_artist.1.into();
        json["participants"] = serde_json::json!({
            "artist": list(artists),
            "albumartist": list(&[album_artist]),
        });
        json
    }

    fn credited(
        id: &str,
        title: &str,
        artists: &[(&str, &str)],
        album_artist: (&str, &str),
    ) -> NativeSong {
        serde_json::from_value(credited_json(id, title, artists, album_artist)).unwrap()
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
        let h = history(library());
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
        let h = history(songs);
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
        let h = history(vec![
            song("1", "A", "starred", 0, None, 0, true),
            song("2", "A", "five", 0, None, 5, false),
            song("3", "A", "starred but disliked", 0, None, 2, true),
            song("4", "A", "middling", 0, None, 3, false),
        ]);
        let favourites = h.favourites().await.unwrap();
        let got: Vec<&str> = favourites.iter().map(|t| t.title.as_str()).collect();
        // Neither has a date, so the higher rating goes first.
        assert_eq!(got, ["five", "starred"]);
    }

    #[tokio::test]
    async fn favourites_put_the_latest_star_or_rating_first() {
        // Navidrome lists songs oldest import first.
        let h = history(vec![
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
        ]);
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
        let known = history(songs).known().await.unwrap();
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
    async fn from_0_55_on_artist_mbids_come_from_the_artist_rows() {
        let sh = ("ar-sh", "Static Harbor");
        let vn = ("ar-vn", "Vela Norte");
        let songs = vec![
            credited("1", "Northern Line", &[sh, vn], ("ar-mc", "Marrow Choir")),
            credited("2", "Undertow", &[sh], sh),
            credited("3", "Foglight", &[("ar-none", "Untagged")], vn),
        ];
        let h = NavidromeHistory::from_songs(songs, artist_rows(), now());

        let artists = h.top_artists(TimePeriod::AllTime, 10).await.unwrap();
        let got: Vec<(&str, Option<&str>, u64)> = artists
            .iter()
            .map(|a| (a.artist.name.as_str(), a.artist.mbid.as_deref(), a.plays))
            .collect();
        // Both Static Harbor songs count under its artist row, whatever the display artist.
        assert_eq!(got, [("Static Harbor", Some(SH), 6), ("Untagged", None, 3)]);
        let tracks = h.top_tracks(TimePeriod::AllTime, 10).await.unwrap();
        let got: Vec<(&str, &str, Option<&str>)> = tracks
            .iter()
            .map(|t| {
                let artist = &t.track.artist;
                (
                    t.track.title.as_str(),
                    artist.name.as_str(),
                    artist.mbid.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("Northern Line", "Static Harbor", Some(SH)),
                ("Undertow", "Static Harbor", Some(SH)),
                ("Foglight", "Untagged", None)
            ]
        );

        let known = h.known().await.unwrap();
        // A recommended track carries its credited artist string.
        assert!(known.knows_track(&TrackRef {
            artist: ArtistRef {
                name: "Static Harbor feat. Vela Norte".into(),
                mbid: None
            },
            title: "Northern Line".into(),
            mbid: None
        }));
        for mbid in [SH, VN, MC] {
            assert!(
                known.knows_artist(&ArtistRef {
                    name: "another spelling".into(),
                    mbid: Some(mbid.to_uppercase())
                }),
                "{mbid}"
            );
        }
        assert!(known.knows_artist(&ArtistRef {
            name: "Vela Norte".into(),
            mbid: None
        }));
    }

    #[tokio::test]
    async fn a_collaboration_and_a_solo_song_name_their_artist_alike() {
        let sh = ("ar-sh", "Static Harbor");
        let vn = ("ar-vn", "Vela Norte");
        let mc = ("ar-mc", "Marrow Choir");
        // The oldest import is the collaboration, last played in January.
        let mut collab = credited("1", "Northern Line", &[sh, vn], sh);
        collab.play_count = 9;
        collab.play_date = Some("2026-01-02T20:00:00Z".parse().unwrap());
        let mut starred = credited("2", "Foglight", &[sh, vn], sh);
        starred.starred = true;
        starred.play_count = 0;
        let solo = credited("3", "Undertow", &[sh], sh);
        let mut newcomer = credited("4", "Lanterns", &[mc], mc);
        newcomer.play_count = 5;
        let h = NavidromeHistory::from_songs(
            vec![collab, starred, solo, newcomer],
            artist_rows(),
            now(),
        );

        let all_time = h.top_artists(TimePeriod::AllTime, 10).await.unwrap();
        let month = h.top_artists(TimePeriod::Month, 10).await.unwrap();
        // This month Static Harbor's only play is the solo song.
        assert_eq!(all_time[0].artist, month[1].artist);
        assert_eq!(all_time[0].artist.name, "Static Harbor");
        let favourites = h.favourites().await.unwrap();
        assert_eq!(favourites[0].artist.name, "Static Harbor");
        let listens = h.listens(10).await.unwrap();
        let got: Vec<&str> = listens.iter().map(|l| l.artist.as_str()).collect();
        assert_eq!(got, ["Static Harbor", "Marrow Choir", "Static Harbor"]);

        let history = Arc::new(h) as Arc<dyn ListeningHistory>;
        let b = build_profile(&[history], &FakeKnowledge::default()).await;
        let momentum: Vec<(&str, f64)> = b
            .profile
            .momentum_artists
            .iter()
            .map(|m| (m.name.as_str(), m.momentum_score))
            .collect();
        // The all-time top artist scores 0 whatever its rank this month.
        assert_eq!(momentum, [("Marrow Choir", 1.0)]);
    }

    #[tokio::test]
    async fn the_songs_own_artist_mbid_goes_first() {
        // A song scanned before Navidrome 0.55.0 carries its own MBID.
        let mut song = credited(
            "1",
            "Northern Line",
            &[("ar-vn", "Vela Norte")],
            ("ar-vn", "Vela Norte"),
        );
        song.mbz_artist_id = Some("a1".into());
        let h = NavidromeHistory::from_songs(vec![song], artist_rows(), now());
        let artists = h.top_artists(TimePeriod::AllTime, 10).await.unwrap();
        assert_eq!(artists[0].artist.mbid.as_deref(), Some("a1"));
    }

    #[tokio::test]
    async fn listens_are_one_per_played_song_newest_first() {
        let h = history(library());
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

    #[tokio::test]
    async fn a_read_takes_the_artist_mbids_from_the_artist_list() {
        let login = r#"{"id":"8uY3tR6eW9qA2sD5fG8hJ1","name":"Alice","username":"alice","isAdmin":false,"token":"eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.e30.c2ln"}"#;
        let sh = ("ar-sh", "Static Harbor");
        let page =
            serde_json::to_string(&[credited_json("1", "Northern Line", &[sh], sh)]).unwrap();
        let artists = serde_json::to_string(&serde_json::json!([
            {"id": "ar-sh", "name": "Static Harbor", "mbzArtistId": SH, "missing": false}
        ]))
        .unwrap();
        let (url, seen) = serve_with_headers(vec![
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
                vec![("X-Total-Count", "1".into())],
            ),
            (
                Reply {
                    status: 200,
                    body: artists,
                },
                vec![],
            ),
        ]);
        let client = NavidromeClientBuilder::new()
            .base_url(&url)
            .username("alice")
            .password("secret-password")
            .build()
            .unwrap();
        let h = read(&client).await.unwrap();
        let top = h.top_artists(TimePeriod::AllTime, 10).await.unwrap();
        assert_eq!(top[0].artist.mbid.as_deref(), Some(SH));
        assert_eq!(seen.lock().unwrap()[2].target, "/api/artist");
    }
}
