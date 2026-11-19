//! Candidates from Deezer artist radios, for users who turned Deezer on.

use std::sync::Arc;

use async_trait::async_trait;
use tracing::info;

use super::{merge_signals, track_candidate};
use crate::deezer::DeezerClient;
use crate::knowledge::KnowledgeError;
use crate::traits::{CandidateGenerator, GeneratorOutput};
use shared::recommendation::{
    fold_name, ArtistRef, CandidateSet, KnownMusic, ProfileConfig, SourceErrors, TrackRef,
    UserMusicProfile,
};

const NAME: &str = "deezer";
const WEIGHT: f64 = 0.4;
const FAVOURITE_SEEDS: usize = 10;

pub struct DeezerGenerator {
    client: Arc<DeezerClient>,
}

impl DeezerGenerator {
    pub fn new(client: Arc<DeezerClient>) -> Self {
        Self { client }
    }

    /// Scores each radio track by its place in the radio: a radio lists its
    /// closest tracks first, so the score falls from the seed weight to half of it.
    async fn radios(
        &self,
        seeds: Vec<(String, f64)>,
        signal: &str,
        known: &KnownMusic,
        errors: &mut SourceErrors,
    ) -> CandidateSet {
        let mut set = CandidateSet::new();
        for (name, weight) in seeds {
            let tracks = match self.radio_of(&name).await {
                Ok(tracks) => tracks,
                Err(e) => {
                    errors.record(e.service(), &e);
                    continue;
                }
            };
            let n = tracks.len() as f64;
            let seed = fold_name(&name);
            for (i, t) in tracks.into_iter().enumerate() {
                let track = TrackRef {
                    artist: ArtistRef {
                        name: t.artist,
                        mbid: None,
                    },
                    title: t.title,
                    mbid: None,
                };
                if fold_name(&track.artist.name) == seed
                    || known.knows_artist(&track.artist)
                    || known.knows_track(&track)
                {
                    continue;
                }
                let score = weight * (1.0 - i as f64 / (2.0 * n));
                set.insert(track_candidate(track, score, signal, NAME));
            }
        }
        set
    }

    async fn radio_of(
        &self,
        name: &str,
    ) -> Result<Vec<crate::deezer::DeezerTrack>, KnowledgeError> {
        match self.client.artist_id(name).await? {
            Some(id) => self.client.artist_radio(id).await,
            None => Ok(Vec::new()),
        }
    }
}

#[async_trait]
impl CandidateGenerator for DeezerGenerator {
    fn name(&self) -> &'static str {
        NAME
    }

    fn weight(&self) -> f64 {
        WEIGHT
    }

    async fn generate(
        &self,
        profile: &UserMusicProfile,
        config: &ProfileConfig,
    ) -> GeneratorOutput {
        let mut errors = SourceErrors::default();
        let radio = self
            .radios(
                radio_seeds(profile),
                "dz_radio",
                &profile.known,
                &mut errors,
            )
            .await;
        let momentum_seeds = profile
            .momentum_artists
            .iter()
            .map(|m| (m.name.clone(), m.momentum_score * config.momentum_weight))
            .collect();
        let momentum = self
            .radios(momentum_seeds, "dz_momentum", &profile.known, &mut errors)
            .await;
        let (candidates, signals) =
            merge_signals(vec![("dz_radio", radio), ("dz_momentum", momentum)]);
        info!("Deezer generator: {} candidates", candidates.len());
        GeneratorOutput {
            candidates,
            signals,
            mbid_failures: 0,
            errors,
        }
    }
}

/// The top artists by share of the most played, then the first favourites at
/// full weight, once per folded name with the higher weight kept.
fn radio_seeds(profile: &UserMusicProfile) -> Vec<(String, f64)> {
    let max = profile
        .seed_artists
        .iter()
        .map(|p| p.plays)
        .max()
        .unwrap_or(0)
        .max(1) as f64;
    let mut seeds: Vec<(String, String, f64)> = Vec::new();
    let candidates = profile
        .seed_artists
        .iter()
        .map(|p| (p.artist.name.as_str(), p.plays as f64 / max))
        .chain(
            profile
                .favourite_artists
                .iter()
                .take(FAVOURITE_SEEDS)
                .map(|a| (a.name.as_str(), 1.0)),
        );
    for (name, weight) in candidates {
        let folded = fold_name(name);
        match seeds.iter_mut().find(|s| s.0 == folded) {
            Some(s) => s.2 = s.2.max(weight),
            None => seeds.push((folded, name.to_string(), weight)),
        }
    }
    seeds.into_iter().map(|(_, name, w)| (name, w)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::test_support::{known_of, played};
    use crate::knowledge::MemoryCache;
    use crate::polite::Pace;
    use crate::test_http::{serve, Reply};
    use shared::recommendation::DiscoveryProfile;

    fn ok(body: &str) -> Reply {
        Reply {
            status: 200,
            body: body.into(),
        }
    }

    #[tokio::test]
    async fn radio_tracks_score_by_position_without_the_seed_or_known_artists() {
        let (base, seen) = serve(vec![ok(include_str!(
            "../deezer/fixtures/radio_fishmans.json"
        ))]);
        let cache = Arc::new(MemoryCache::default());
        cache.put_now("kc1:deezer:artist:fishmans", "13628349");
        let g = DeezerGenerator::new(Arc::new(
            DeezerClient::new(&base, Pace::immediate(), cache).unwrap(),
        ));
        let profile = UserMusicProfile {
            seed_artists: vec![played("Fishmans", None, 10)],
            known: known_of(&["Stereolab"]),
            ..Default::default()
        };
        let out = g
            .generate(
                &profile,
                &ProfileConfig::from_profile(DiscoveryProfile::Balanced),
            )
            .await;
        let mut got: Vec<_> = out
            .candidates
            .into_vec()
            .into_iter()
            .map(|c| (c.artist, (c.score * 1e4).round() / 1e4))
            .collect();
        got.sort_by(|a, b| b.1.total_cmp(&a.1));
        assert_eq!(
            got,
            [
                ("Userband".to_string(), 0.9167),
                ("Black Moth Super Rainbow".to_string(), 0.75)
            ]
        );
        assert_eq!(
            seen.lock().unwrap()[0].target,
            "/artist/13628349/radio?limit=100"
        );
    }
}
