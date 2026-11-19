//! Candidates from the user's own ListenBrainz account: what the listeners
//! closest to them play, and the playlists Troi made for them.

use std::sync::Arc;

use async_trait::async_trait;
use tracing::info;

use super::{merge_signals, track_candidate};
use crate::listenbrainz::client::ListenBrainzClient;
use crate::polite::RemoteError;
use crate::traits::{CandidateGenerator, GeneratorOutput};
use shared::recommendation::{
    ArtistRef, CandidateSet, KnownMusic, ProfileConfig, SourceErrors, TimePeriod, TrackRef,
    UserMusicProfile,
};

const NAME: &str = "listenbrainz_account";
const SIMILAR_USERS: usize = 10;
const RECORDINGS_PER_USER: usize = 50;
/// ln(0) is undefined, so a recording without listens still weighs a little.
const NO_LISTENS_WEIGHT: f64 = 0.1;
/// Troi orders a playlist best first, so scores fall from 1.0 toward 0.5 along it.
const TROI_DECAY: f64 = 0.5;

pub struct ListenBrainzAccountGenerator {
    client: Arc<ListenBrainzClient>,
    user: String,
}

impl ListenBrainzAccountGenerator {
    pub fn new(client: Arc<ListenBrainzClient>, user: impl Into<String>) -> Self {
        Self {
            client,
            user: user.into(),
        }
    }

    /// The recent top recordings of the most similar users, weighted by how
    /// close each user is and how often they played the recording; a
    /// recording several of them play adds up their shares.
    async fn collab_filter(&self, known: &KnownMusic, errors: &mut SourceErrors) -> CandidateSet {
        let mut set = CandidateSet::new();
        let users = match self.client.similar_users(&self.user).await {
            Ok(users) => users,
            Err(e) => {
                failed(errors, &e);
                return set;
            }
        };
        for other in users.iter().take(SIMILAR_USERS) {
            let recordings = match self
                .client
                .user_top_recordings(&other.user_name, TimePeriod::Quarter, RECORDINGS_PER_USER)
                .await
            {
                Ok(recordings) => recordings,
                Err(e) => {
                    failed(errors, &e);
                    continue;
                }
            };
            for rec in recordings {
                let listens = match rec.listen_count {
                    0 => NO_LISTENS_WEIGHT,
                    n => (n as f64).ln(),
                };
                let track = TrackRef {
                    artist: ArtistRef {
                        name: rec.artist_name,
                        mbid: rec.artist_mbids.into_iter().next(),
                    },
                    title: rec.track_name,
                    mbid: rec.recording_mbid,
                };
                if is_new(known, &track) {
                    let score = other.similarity * listens;
                    set.insert(track_candidate(track, score, "lb_collab_filter", NAME));
                }
            }
        }
        set
    }

    /// The tracks of every playlist Troi made for the user, by position.
    async fn troi(&self, known: &KnownMusic, errors: &mut SourceErrors) -> CandidateSet {
        let mut set = CandidateSet::new();
        let playlists = match self.client.recommendation_playlists(&self.user).await {
            Ok(playlists) => playlists,
            Err(e) => {
                failed(errors, &e);
                return set;
            }
        };
        for summary in playlists {
            let tracks = match self.client.playlist(&summary.mbid).await {
                Ok(tracks) => tracks,
                Err(e) => {
                    failed(errors, &e);
                    continue;
                }
            };
            let len = tracks.len() as f64;
            for (i, t) in tracks.into_iter().enumerate() {
                let track = TrackRef {
                    artist: ArtistRef {
                        name: t.creator,
                        mbid: t.artist_mbid,
                    },
                    title: t.title,
                    mbid: t.recording_mbid,
                };
                if is_new(known, &track) {
                    let score = 1.0 - TROI_DECAY * i as f64 / len;
                    set.insert(track_candidate(track, score, "lb_troi", NAME));
                }
            }
        }
        set
    }
}

#[async_trait]
impl CandidateGenerator for ListenBrainzAccountGenerator {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn generate(
        &self,
        profile: &UserMusicProfile,
        _config: &ProfileConfig,
    ) -> GeneratorOutput {
        let mut errors = SourceErrors::default();
        let collab = self.collab_filter(&profile.known, &mut errors).await;
        let troi = self.troi(&profile.known, &mut errors).await;
        let (candidates, signals) =
            merge_signals(vec![("lb_collab_filter", collab), ("lb_troi", troi)]);
        info!(
            "ListenBrainz account generator: {} candidates",
            candidates.len()
        );
        GeneratorOutput {
            candidates,
            signals,
            mbid_failures: 0,
            errors,
        }
    }
}

fn is_new(known: &KnownMusic, track: &TrackRef) -> bool {
    !known.knows_artist(&track.artist) && !known.knows_track(track)
}

fn failed(errors: &mut SourceErrors, e: &RemoteError) {
    errors.record(e.service(), e);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::test_support::known_of;
    use crate::polite::Pace;
    use crate::test_http::{serve, Reply};
    use shared::recommendation::{CandidateSet, DiscoveryProfile};

    const SIMILAR_USERS: &str = include_str!("../listenbrainz/fixtures/similar_users_rob.json");
    const TOP_RECORDINGS: &str =
        include_str!("../listenbrainz/fixtures/user_top_recordings_rob.json");
    const PLAYLISTS: &str =
        include_str!("../listenbrainz/fixtures/recommendation_playlists_rob.json");
    const PLAYLIST: &str = include_str!("../listenbrainz/fixtures/playlist.json");

    fn ok(body: &str) -> Reply {
        Reply {
            status: 200,
            body: body.into(),
        }
    }

    /// The output and the target of every request, in order.
    async fn run(
        replies: Vec<Reply>,
        profile: &UserMusicProfile,
    ) -> (GeneratorOutput, Vec<String>) {
        let (base, seen) = serve(replies);
        let client = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();
        let config = ProfileConfig::from_profile(DiscoveryProfile::Balanced);
        let out = ListenBrainzAccountGenerator::new(Arc::new(client), "rob")
            .generate(profile, &config)
            .await;
        let targets = seen
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.target.clone())
            .collect();
        (out, targets)
    }

    #[tokio::test]
    async fn collab_scores_by_similarity_and_listens_and_skips_known_music() {
        let profile = UserMusicProfile {
            known: known_of(&["TM Juke"]),
            ..Default::default()
        };
        let (out, targets) = run(
            vec![
                ok(SIMILAR_USERS),
                ok(TOP_RECORDINGS),
                ok(TOP_RECORDINGS),
                ok(PLAYLISTS),
                ok(PLAYLIST),
            ],
            &profile,
        )
        .await;
        assert_eq!(
            targets[..3],
            [
                "/1/user/rob/similar-users",
                "/1/stats/user/Violet%26Red/recordings?range=quarter&count=50",
                "/1/stats/user/mcc1979/recordings?range=quarter&count=50",
            ]
        );
        let candidates = &out.candidates.candidates;
        assert!(!candidates.contains_key(&CandidateSet::key("TM Juke", "Just for a Day (Sunday)")));
        let moving = &candidates[&CandidateSet::key("Pretty Lights", "Finally Moving")];
        // Both similar users play it, so both their shares count.
        let expected = (0.5391550135659047 + 0.3320996942266951) * 227f64.ln();
        assert!((moving.score - expected).abs() < 1e-9, "{}", moving.score);
        assert_eq!(moving.signals, ["lb_collab_filter"]);
        assert_eq!(moving.source, "listenbrainz_account");
        assert_eq!(
            moving.mbid.as_deref(),
            Some("3b3aaac8-64e5-4574-8290-3002d199fb9c")
        );
        assert_eq!(
            moving.artist_mbid.as_deref(),
            Some("3a07764c-7cc5-42e9-8205-3de7532f4771")
        );
        let signals: Vec<_> = out
            .signals
            .iter()
            .map(|s| (s.name.as_str(), s.candidates_produced))
            .collect();
        assert_eq!(signals, [("lb_collab_filter", 1), ("lb_troi", 3)]);
        assert!(out.errors.is_empty());
    }

    #[tokio::test]
    async fn troi_reads_each_playlists_tracks() {
        let mut profile = UserMusicProfile::default();
        profile
            .known
            .recording_mbids
            .insert("418bce6c-1195-4cf2-b5c0-4cf163acbd46".into());
        let (out, targets) = run(
            vec![ok(r#"{"payload":[]}"#), ok(PLAYLISTS), ok(PLAYLIST)],
            &profile,
        )
        .await;
        assert_eq!(
            targets,
            [
                "/1/user/rob/similar-users",
                "/1/user/rob/playlists/recommendations",
                "/1/playlist/48f2eb97-33e7-496c-8da1-25ead7280a81",
            ]
        );
        let mut got: Vec<_> = out
            .candidates
            .into_vec()
            .into_iter()
            .map(|c| (c.track, (c.score * 1e6).round() / 1e6, c.signals))
            .collect();
        got.sort_by(|a, b| b.1.total_cmp(&a.1));
        // Lido, second of three at 1.0 - 0.5 / 3, is known; positions still count it.
        assert_eq!(
            got,
            [
                ("Fauna".to_string(), 1.0, vec!["lb_troi".to_string()]),
                (
                    "Memory Arc".to_string(),
                    0.666667,
                    vec!["lb_troi".to_string()]
                ),
            ]
        );
    }
}
