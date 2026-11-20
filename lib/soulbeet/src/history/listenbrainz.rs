//! Listening history read from a ListenBrainz account. Statistics, listens and loved tracks of a
//! public profile need no token.

use std::sync::Arc;

use async_trait::async_trait;
use shared::recommendation::{
    ArtistRef, KnownMusic, Listen, PlayedArtist, PlayedTrack, TimePeriod, TrackRef,
};

use super::HistoryError;
use crate::listenbrainz::client::ListenBrainzClient;
use crate::listenbrainz::models::LbRecording;
use crate::traits::ListeningHistory;

pub struct ListenBrainzHistory {
    client: Arc<ListenBrainzClient>,
    user: String,
}

impl ListenBrainzHistory {
    pub fn new(client: Arc<ListenBrainzClient>, user: impl Into<String>) -> Self {
        Self {
            client,
            user: user.into(),
        }
    }

    async fn artists(
        &self,
        period: TimePeriod,
        count: usize,
    ) -> Result<Vec<PlayedArtist>, HistoryError> {
        Ok(self
            .client
            .user_top_artists(&self.user, period, count)
            .await?
            .into_iter()
            .map(|a| PlayedArtist {
                artist: ArtistRef {
                    name: a.artist_name,
                    mbid: a.artist_mbid,
                },
                plays: a.listen_count,
            })
            .collect())
    }

    async fn tracks(
        &self,
        period: TimePeriod,
        count: usize,
    ) -> Result<Vec<PlayedTrack>, HistoryError> {
        Ok(self
            .client
            .user_top_recordings(&self.user, period, count)
            .await?
            .into_iter()
            .map(played_track)
            .collect())
    }
}

fn played_track(r: LbRecording) -> PlayedTrack {
    PlayedTrack {
        track: TrackRef {
            artist: ArtistRef {
                name: r.artist_name,
                mbid: r.artist_mbids.first().cloned(),
            },
            title: r.track_name,
            mbid: r.recording_mbid,
        },
        plays: r.listen_count,
    }
}

#[async_trait]
impl ListeningHistory for ListenBrainzHistory {
    fn name(&self) -> &'static str {
        "ListenBrainz"
    }

    async fn top_artists(
        &self,
        period: TimePeriod,
        count: usize,
    ) -> Result<Vec<PlayedArtist>, HistoryError> {
        self.artists(period, count).await
    }

    async fn top_tracks(
        &self,
        period: TimePeriod,
        count: usize,
    ) -> Result<Vec<PlayedTrack>, HistoryError> {
        self.tracks(period, count).await
    }

    async fn listens(&self, count: usize) -> Result<Vec<Listen>, HistoryError> {
        Ok(self
            .client
            .user_listens(&self.user, count)
            .await?
            .into_iter()
            .map(|l| Listen {
                artist: l.track_metadata.artist_name,
                track: l.track_metadata.track_name,
                album: l.track_metadata.release_name,
                timestamp: l.listened_at,
            })
            .collect())
    }

    async fn favourites(&self) -> Result<Vec<TrackRef>, HistoryError> {
        Ok(self
            .client
            .user_loved(&self.user)
            .await?
            .into_iter()
            .filter_map(|f| {
                let meta = f.track_metadata?;
                let mapping = meta.mbid_mapping;
                Some(TrackRef {
                    artist: ArtistRef {
                        name: meta.artist_name,
                        mbid: mapping
                            .as_ref()
                            .and_then(|m| m.artist_mbids.first().cloned()),
                    },
                    title: meta.track_name,
                    mbid: mapping.and_then(|m| m.recording_mbid),
                })
            })
            .collect())
    }

    async fn known(&self) -> Result<KnownMusic, HistoryError> {
        let mut known = KnownMusic::default();
        for played in self.artists(TimePeriod::AllTime, 200).await? {
            known.add_artist(&played.artist);
        }
        for played in self.tracks(TimePeriod::AllTime, 200).await? {
            known.add_track(&played.track);
        }
        Ok(known)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::polite::{Pace, RemoteError};
    use crate::test_http::{serve, Reply};

    fn ok(body: &str) -> Reply {
        Reply {
            status: 200,
            body: body.into(),
        }
    }

    #[tokio::test]
    async fn top_artists_and_tracks_carry_their_mbids() {
        let (base, _) = serve(vec![
            ok(include_str!(
                "../listenbrainz/fixtures/user_top_artists_rob.json"
            )),
            ok(include_str!(
                "../listenbrainz/fixtures/user_top_recordings_rob.json"
            )),
        ]);
        let client = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();
        let h = ListenBrainzHistory::new(Arc::new(client), "rob");
        let artists = h.top_artists(TimePeriod::AllTime, 5).await.unwrap();
        assert_eq!(
            artists[0],
            PlayedArtist {
                artist: ArtistRef {
                    name: "Wax Tailor".into(),
                    mbid: Some("c24b47a1-2f52-44f3-a3c6-374dca844731".into()),
                },
                plays: 4298,
            }
        );
        let tracks = h.top_tracks(TimePeriod::AllTime, 5).await.unwrap();
        assert_eq!(tracks[0].track.title, "Just for a Day (Sunday)");
        assert_eq!(
            tracks[0].track.artist.mbid.as_deref(),
            Some("3fa17b15-b8d1-46c4-a0e3-6e0f2a064a1d")
        );
        assert_eq!(
            tracks[0].track.mbid.as_deref(),
            Some("586a6fc2-804e-4e60-a115-217d8ae9e90d")
        );
    }

    #[tokio::test]
    async fn loved_tracks_are_favourites() {
        let (base, _) = serve(vec![ok(include_str!(
            "../listenbrainz/fixtures/loved_rob.json"
        ))]);
        let client = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();
        let h = ListenBrainzHistory::new(Arc::new(client), "rob");
        let loved = h.favourites().await.unwrap();
        assert_eq!(loved.len(), 2);
        assert_eq!(loved[0].title, "Heaven Knows (DNMO remix)");
        assert_eq!(
            loved[0].mbid.as_deref(),
            Some("40313583-d07e-4ed3-a2db-1a6d3f63906e")
        );
        assert_eq!(
            loved[0].artist.mbid.as_deref(),
            Some("29ed4a49-fb99-4a5c-8713-609cabe6f34a")
        );
    }

    #[tokio::test]
    async fn known_music_is_the_all_time_top_lists() {
        let (base, seen) = serve(vec![
            ok(include_str!(
                "../listenbrainz/fixtures/user_top_artists_rob.json"
            )),
            ok(include_str!(
                "../listenbrainz/fixtures/user_top_recordings_rob.json"
            )),
        ]);
        let client = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();
        let h = ListenBrainzHistory::new(Arc::new(client), "rob");
        let known = h.known().await.unwrap();
        let targets: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.target.clone())
            .collect();
        assert_eq!(
            targets,
            [
                "/1/stats/user/rob/artists?range=all_time&count=200",
                "/1/stats/user/rob/recordings?range=all_time&count=200",
            ]
        );
        assert!(known
            .artist_mbids
            .contains("c24b47a1-2f52-44f3-a3c6-374dca844731"));
        assert!(known
            .recording_mbids
            .contains("586a6fc2-804e-4e60-a115-217d8ae9e90d"));
    }

    #[tokio::test]
    async fn a_listenbrainz_outage_is_a_history_error() {
        let bad = || Reply {
            status: 502,
            body: String::new(),
        };
        let (base, _) = serve(vec![bad(), bad(), bad(), bad()]);
        let client = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();
        let h = ListenBrainzHistory::new(Arc::new(client), "rob");
        let err = h.favourites().await.unwrap_err();
        assert!(matches!(
            err,
            HistoryError::Remote(RemoteError::Unavailable {
                service: "ListenBrainz",
                ..
            })
        ));
        assert_eq!(err.service(), "ListenBrainz");
    }
}
