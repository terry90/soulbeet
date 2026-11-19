//! Candidates from the music knowledge alone: the neighbours of the artists
//! the user plays and stars, of the artists gaining in their listening, and
//! the tracks of tags next to their taste.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use tracing::info;

use super::{cap_signal, signal_report};
use crate::knowledge::KnowledgeError;
use crate::traits::{CandidateGenerator, GeneratorOutput};
use crate::MusicKnowledge;
use shared::recommendation::{
    ArtistRef, Candidate, CandidateSet, KnownMusic, Neighbour, ProfileConfig, SourceErrors,
    TrackRef, UserMusicProfile,
};

const NAME: &str = "catalog";
const FAVOURITE_SEEDS: usize = 10;
const HOP1_NEIGHBOURS: usize = 5;
/// Only the closest neighbours of a seed are followed a second hop.
const HOP2_FROM: usize = 3;
const HOP2_NEIGHBOURS: usize = 3;
const MOMENTUM_NEIGHBOURS: usize = 10;
const TRACKS_PER_ARTIST: usize = 3;
const EXPLORATION_TAGS: usize = 5;
const TRACKS_PER_TAG: usize = 20;
const WEIGHT_HOP1: f64 = 0.7;
const WEIGHT_TAG_EXPLORE: f64 = 0.5;
/// Lets exploration candidates compete in the diversifier instead of only backfilling.
const MIN_TAG_EXPLORE_SCORE: f64 = 0.15;
/// The weight of an exploration tag the user's genres do not carry.
const ABSENT_TAG_WEIGHT: f64 = 0.1;

pub struct CatalogGenerator {
    knowledge: Arc<dyn MusicKnowledge>,
}

impl CatalogGenerator {
    pub fn new(knowledge: Arc<dyn MusicKnowledge>) -> Self {
        Self { knowledge }
    }
}

#[async_trait]
impl CandidateGenerator for CatalogGenerator {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn generate(
        &self,
        profile: &UserMusicProfile,
        config: &ProfileConfig,
    ) -> GeneratorOutput {
        let mut run = Run {
            knowledge: self.knowledge.as_ref(),
            known: &profile.known,
            errors: SourceErrors::default(),
        };
        let (hop1, hop2) = run.artist_hops(profile, config).await;
        let momentum = run.momentum(profile, config).await;
        let tag_explore = run.tag_explore(profile).await;

        let mut candidates = CandidateSet::new();
        let mut signals = Vec::new();
        for (name, mut set) in [
            ("artist_hop1", hop1),
            ("artist_hop2", hop2),
            ("momentum", momentum),
            ("tag_explore", tag_explore),
        ] {
            cap_signal(&mut set);
            signals.push(signal_report(name, &set));
            for candidate in set.into_vec() {
                candidates.insert(candidate);
            }
        }
        info!("Catalog generator: {} candidates", candidates.len());
        GeneratorOutput {
            candidates,
            signals,
            mbid_failures: 0,
            errors: run.errors,
        }
    }
}

/// One generation: what to leave out and the failures met on the way.
struct Run<'a> {
    knowledge: &'a dyn MusicKnowledge,
    known: &'a KnownMusic,
    errors: SourceErrors,
}

impl Run<'_> {
    /// Hop 1 follows each seed to its neighbours, hop 2 the closest of those
    /// to theirs, never back to the seed.
    async fn artist_hops(
        &mut self,
        profile: &UserMusicProfile,
        config: &ProfileConfig,
    ) -> (CandidateSet, CandidateSet) {
        let mut hop1 = CandidateSet::new();
        let mut hop2 = CandidateSet::new();
        for seed in seeds(profile) {
            let neighbours = self.similar(&seed, HOP1_NEIGHBOURS).await;
            for n in &neighbours {
                if !self.known.knows_artist(&n.artist) {
                    let score = n.score * WEIGHT_HOP1;
                    self.add_top_tracks(&mut hop1, &n.artist, score, "artist_hop1")
                        .await;
                }
            }
            for n in neighbours.iter().take(HOP2_FROM) {
                for second in self.similar(&n.artist, HOP2_NEIGHBOURS).await {
                    if self.known.knows_artist(&second.artist) || same_mbid(&second.artist, &seed) {
                        continue;
                    }
                    let score = n.score * second.score * config.hop2_weight;
                    self.add_top_tracks(&mut hop2, &second.artist, score, "artist_hop2")
                        .await;
                }
            }
        }
        (hop1, hop2)
    }

    async fn momentum(
        &mut self,
        profile: &UserMusicProfile,
        config: &ProfileConfig,
    ) -> CandidateSet {
        let mut set = CandidateSet::new();
        for m in &profile.momentum_artists {
            let Some(mbid) = &m.mbid else { continue };
            let artist = ArtistRef {
                name: m.name.clone(),
                mbid: Some(mbid.clone()),
            };
            for n in self.similar(&artist, MOMENTUM_NEIGHBOURS).await {
                if self.known.knows_artist(&n.artist) {
                    continue;
                }
                let score = m.momentum_score * n.score * config.momentum_weight;
                self.add_top_tracks(&mut set, &n.artist, score, "momentum")
                    .await;
            }
        }
        set
    }

    async fn tag_explore(&mut self, profile: &UserMusicProfile) -> CandidateSet {
        let mut set = CandidateSet::new();
        let weights: HashMap<String, f64> = profile
            .genre_distribution
            .iter()
            .map(|t| (t.name.to_lowercase(), t.weight))
            .collect();
        for tag in profile.tag_exploration_zone.iter().take(EXPLORATION_TAGS) {
            let tracks = match self.knowledge.tag_tracks(tag, TRACKS_PER_TAG).await {
                Ok(tracks) => tracks,
                Err(e) => {
                    self.failed(&e);
                    continue;
                }
            };
            let weight = weights
                .get(&tag.to_lowercase())
                .copied()
                .unwrap_or(ABSENT_TAG_WEIGHT);
            let score = (WEIGHT_TAG_EXPLORE * weight).max(MIN_TAG_EXPLORE_SCORE);
            for track in tracks {
                if self.known.knows_artist(&track.artist) || self.known.knows_track(&track) {
                    continue;
                }
                set.insert(candidate(track, score, "tag_explore"));
            }
        }
        set
    }

    /// Empty for an artist without an MBID, which the knowledge cannot look up.
    async fn similar(&mut self, artist: &ArtistRef, limit: usize) -> Vec<Neighbour> {
        let Some(mbid) = &artist.mbid else {
            return Vec::new();
        };
        match self
            .knowledge
            .similar_artists(&artist.name, mbid, limit)
            .await
        {
            Ok(neighbours) => neighbours,
            Err(e) => {
                self.failed(&e);
                Vec::new()
            }
        }
    }

    async fn add_top_tracks(
        &mut self,
        set: &mut CandidateSet,
        artist: &ArtistRef,
        score: f64,
        signal: &str,
    ) {
        let Some(mbid) = &artist.mbid else { return };
        match self
            .knowledge
            .artist_top_tracks(mbid, TRACKS_PER_ARTIST)
            .await
        {
            Ok(tracks) => {
                for track in tracks {
                    if !self.known.knows_track(&track) {
                        set.insert(candidate(track, score, signal));
                    }
                }
            }
            Err(e) => self.failed(&e),
        }
    }

    fn failed(&mut self, e: &KnowledgeError) {
        self.errors.record(e.service(), e);
    }
}

/// The top artists, then the first favourite artists, once per MBID; an
/// artist without one has no neighbours to ask for.
fn seeds(profile: &UserMusicProfile) -> Vec<ArtistRef> {
    let mut seen = HashSet::new();
    profile
        .seed_artists
        .iter()
        .map(|p| &p.artist)
        .chain(profile.favourite_artists.iter().take(FAVOURITE_SEEDS))
        .filter(|a| {
            a.mbid
                .as_ref()
                .is_some_and(|m| seen.insert(m.to_lowercase()))
        })
        .cloned()
        .collect()
}

fn same_mbid(a: &ArtistRef, b: &ArtistRef) -> bool {
    matches!((&a.mbid, &b.mbid), (Some(x), Some(y)) if x.eq_ignore_ascii_case(y))
}

fn candidate(track: TrackRef, score: f64, signal: &str) -> Candidate {
    Candidate {
        artist: track.artist.name,
        track: track.title,
        album: None,
        mbid: track.mbid,
        score,
        signals: vec![signal.to_string()],
        source: NAME.to_string(),
        artist_listener_count: None,
        primary_genre: None,
        release_year: None,
        artist_mbid: track.artist.mbid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::test_support::*;
    use shared::recommendation::{DiscoveryProfile, MomentumArtist, PlayedArtist};

    const FISHMANS: &str = "d0f25f16-7782-42e8-8047-c16a1cb2b450";

    fn neighbour(name: &str, mbid: &str, score: f64) -> Neighbour {
        Neighbour {
            artist: artist(name, Some(mbid)),
            score,
        }
    }

    fn seeded(seeds: &[(&str, &str)]) -> UserMusicProfile {
        UserMusicProfile {
            seed_artists: seeds
                .iter()
                .map(|(name, mbid)| played(name, Some(mbid), 10))
                .collect::<Vec<PlayedArtist>>(),
            ..Default::default()
        }
    }

    async fn run(knowledge: FakeKnowledge, profile: &UserMusicProfile) -> GeneratorOutput {
        let config = ProfileConfig::from_profile(DiscoveryProfile::Balanced);
        CatalogGenerator::new(Arc::new(knowledge))
            .generate(profile, &config)
            .await
    }

    /// (artist, track, score rounded to 1e-6, signals), sorted.
    fn found(out: &GeneratorOutput) -> Vec<(String, String, f64, Vec<String>)> {
        let mut found: Vec<_> = out
            .candidates
            .candidates
            .values()
            .map(|c| {
                (
                    c.artist.clone(),
                    c.track.clone(),
                    (c.score * 1e6).round() / 1e6,
                    c.signals.clone(),
                )
            })
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        found
    }

    fn one(
        artist: &str,
        track: &str,
        score: f64,
        signal: &str,
    ) -> (String, String, f64, Vec<String>) {
        (
            artist.to_string(),
            track.to_string(),
            score,
            vec![signal.to_string()],
        )
    }

    #[tokio::test]
    async fn hop1_takes_each_unknown_neighbours_top_tracks() {
        let mut profile = seeded(&[("A", "a")]);
        profile.known.add_artist(&artist("C", Some("c")));
        profile
            .known
            .track_keys
            .insert(CandidateSet::key("B", "b1"));
        let k = FakeKnowledge {
            neighbours: [(
                "a",
                vec![neighbour("B", "b", 1.0), neighbour("C", "c", 0.5)],
            )]
            .into(),
            top_tracks: [
                (
                    "b",
                    vec![
                        track("B", Some("b"), "b1", None),
                        track("B", Some("b"), "b2", None),
                    ],
                ),
                ("c", vec![track("C", Some("c"), "c1", None)]),
            ]
            .into(),
            ..Default::default()
        };
        let out = run(k, &profile).await;
        assert_eq!(found(&out), [one("B", "b2", 0.7, "artist_hop1")]);
        let c = out.candidates.candidates.values().next().unwrap();
        assert_eq!(c.source, "catalog");
        assert_eq!(c.artist_mbid.as_deref(), Some("b"));
        let signals: Vec<_> = out.signals.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            signals,
            ["artist_hop1", "artist_hop2", "momentum", "tag_explore"]
        );
        assert_eq!(out.signals[0].candidates_produced, 1);
        assert!(out.errors.is_empty());
    }

    #[tokio::test]
    async fn hop2_scales_by_both_hops_and_never_returns_to_the_seed() {
        let profile = seeded(&[("A", "a")]);
        let k = FakeKnowledge {
            neighbours: [
                ("a", vec![neighbour("B", "b", 1.0)]),
                (
                    "b",
                    vec![neighbour("A", "a", 1.0), neighbour("D", "d", 0.5)],
                ),
            ]
            .into(),
            top_tracks: [
                ("a", vec![track("A", Some("a"), "a1", None)]),
                ("d", vec![track("D", Some("d"), "d1", None)]),
            ]
            .into(),
            ..Default::default()
        };
        let out = run(k, &profile).await;
        assert_eq!(found(&out), [one("D", "d1", 0.25, "artist_hop2")]);
    }

    #[tokio::test]
    async fn momentum_and_tag_explore_follow_their_weights() {
        let mut profile = UserMusicProfile {
            momentum_artists: vec![MomentumArtist {
                name: "M".into(),
                momentum_score: 0.8,
                mbid: Some("m".into()),
            }],
            genre_distribution: vec![tag("slowcore", 0.6), tag("shoegaze", 0.02)],
            tag_exploration_zone: vec!["shoegaze".into(), "slowcore".into()],
            ..Default::default()
        };
        profile.known.add_artist(&artist("K", None));
        let k = FakeKnowledge {
            neighbours: [("m", vec![neighbour("N", "n", 0.5)])].into(),
            top_tracks: [("n", vec![track("N", Some("n"), "n1", None)])].into(),
            tag_tracks: [
                (
                    "shoegaze",
                    vec![track("S", None, "s1", None), track("K", None, "k1", None)],
                ),
                ("slowcore", vec![track("L", None, "l1", None)]),
            ]
            .into(),
            ..Default::default()
        };
        let out = run(k, &profile).await;
        assert_eq!(
            found(&out),
            [
                one("L", "l1", 0.3, "tag_explore"),
                one("N", "n1", 0.28, "momentum"),
                one("S", "s1", 0.15, "tag_explore"),
            ]
        );
    }

    #[tokio::test]
    async fn a_library_artist_under_another_name_is_skipped() {
        let mut profile = seeded(&[("A", "a")]);
        profile
            .known
            .add_artist(&artist("フィッシュマンズ", Some(FISHMANS)));
        let k = FakeKnowledge {
            neighbours: [("a", vec![neighbour("Fishmans", FISHMANS, 1.0)])].into(),
            top_tracks: [(
                FISHMANS,
                vec![track("Fishmans", Some(FISHMANS), "Long Season", None)],
            )]
            .into(),
            ..Default::default()
        };
        let out = run(k, &profile).await;
        assert!(found(&out).is_empty(), "{:?}", found(&out));
    }

    #[tokio::test]
    async fn a_failing_lookup_is_reported_and_the_rest_still_runs() {
        let profile = seeded(&[("A", "a"), ("E", "e")]);
        let k = FakeKnowledge {
            neighbours: [
                ("a", vec![neighbour("B", "b", 1.0)]),
                ("e", vec![neighbour("F", "f", 1.0)]),
            ]
            .into(),
            top_tracks: [
                ("b", vec![track("B", Some("b"), "b1", None)]),
                ("f", vec![track("F", Some("f"), "f1", None)]),
            ]
            .into(),
            failing: ["similar_artists:a".to_string()].into(),
            ..Default::default()
        };
        let out = run(k, &profile).await;
        assert_eq!(found(&out), [one("F", "f1", 0.7, "artist_hop1")]);
        let errors = out.errors.into_vec();
        assert_eq!(errors.len(), 1);
        assert_eq!(
            (errors[0].source.as_str(), errors[0].failures),
            ("ListenBrainz Labs", 1)
        );
    }
}
