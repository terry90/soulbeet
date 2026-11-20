//! In-memory listening histories, music knowledge and generators for engine tests.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use shared::recommendation::{
    fold_name, ArtistRef, Candidate, CandidateSet, DiscoveryProfile, KnownMusic, Listen, Neighbour,
    PlayedArtist, PlayedTrack, ProfileConfig, RecordingFacts, SourceErrors, TimePeriod, TrackRef,
    UserMusicProfile, WeightedTag,
};

use super::track_candidate;
use crate::history::HistoryError;
use crate::knowledge::KnowledgeError;
use crate::polite::RemoteError;
use crate::{CandidateGenerator, GeneratorOutput, ListeningHistory, MusicKnowledge};

#[derive(Default)]
pub(crate) struct FakeHistory {
    pub name: &'static str,
    pub top_artists: Vec<PlayedArtist>,
    pub month_artists: Vec<PlayedArtist>,
    pub top_tracks: Vec<PlayedTrack>,
    pub listens: Vec<Listen>,
    pub favourites: Vec<TrackRef>,
    pub known: KnownMusic,
    pub fails: bool,
}

impl FakeHistory {
    fn answer<T: Clone>(&self, items: &[T], count: usize) -> Result<Vec<T>, HistoryError> {
        self.check()?;
        Ok(items.iter().take(count).cloned().collect())
    }

    fn check(&self) -> Result<(), HistoryError> {
        if self.fails {
            return Err(RemoteError::Unavailable {
                service: self.name,
                detail: "down".into(),
            }
            .into());
        }
        Ok(())
    }
}

#[async_trait]
impl ListeningHistory for FakeHistory {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn top_artists(
        &self,
        period: TimePeriod,
        count: usize,
    ) -> Result<Vec<PlayedArtist>, HistoryError> {
        match period {
            TimePeriod::Month => self.answer(&self.month_artists, count),
            _ => self.answer(&self.top_artists, count),
        }
    }

    async fn top_tracks(
        &self,
        _period: TimePeriod,
        count: usize,
    ) -> Result<Vec<PlayedTrack>, HistoryError> {
        self.answer(&self.top_tracks, count)
    }

    async fn listens(&self, count: usize) -> Result<Vec<Listen>, HistoryError> {
        self.answer(&self.listens, count)
    }

    async fn favourites(&self) -> Result<Vec<TrackRef>, HistoryError> {
        self.answer(&self.favourites, usize::MAX)
    }

    async fn known(&self) -> Result<KnownMusic, HistoryError> {
        self.check()?;
        Ok(self.known.clone())
    }
}

/// Answers from maps keyed by folded artist name (`mbids`), tag or MBID.
#[derive(Default)]
pub(crate) struct FakeKnowledge {
    pub mbids: HashMap<&'static str, &'static str>,
    pub neighbours: HashMap<&'static str, Vec<Neighbour>>,
    pub top_tracks: HashMap<&'static str, Vec<TrackRef>>,
    pub tags: HashMap<&'static str, Vec<WeightedTag>>,
    pub users: HashMap<&'static str, u64>,
    pub reference: u64,
    pub related: HashMap<&'static str, Vec<&'static str>>,
    pub tag_tracks: HashMap<&'static str, Vec<TrackRef>>,
    pub recordings: HashMap<&'static str, RecordingFacts>,
    /// `"{method}"` fails every call of a method, `"{method}:{key}"` one key.
    pub failing: HashSet<String>,
}

impl FakeKnowledge {
    fn check<'a>(
        &self,
        method: &str,
        keys: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), KnowledgeError> {
        let fails = self.failing.contains(method)
            || keys
                .into_iter()
                .any(|k| self.failing.contains(&format!("{method}:{k}")));
        if fails {
            return Err(RemoteError::Unavailable {
                service: "ListenBrainz Labs",
                detail: "down".into(),
            }
            .into());
        }
        Ok(())
    }

    fn each<T: Clone>(
        &self,
        method: &str,
        map: &HashMap<&'static str, T>,
        mbids: &[String],
    ) -> Result<HashMap<String, T>, KnowledgeError> {
        self.check(method, mbids.iter().map(String::as_str))?;
        Ok(mbids
            .iter()
            .filter_map(|m| Some((m.clone(), map.get(m.as_str())?.clone())))
            .collect())
    }
}

fn listed<T: Clone>(map: &HashMap<&'static str, Vec<T>>, key: &str, limit: usize) -> Vec<T> {
    map.get(key)
        .map(|v| v.iter().take(limit).cloned().collect())
        .unwrap_or_default()
}

#[async_trait]
impl MusicKnowledge for FakeKnowledge {
    fn name(&self) -> &'static str {
        "fake knowledge"
    }

    async fn resolve_artist(&self, name: &str) -> Result<Option<String>, KnowledgeError> {
        let key = fold_name(name);
        self.check("resolve_artist", [key.as_str()])?;
        Ok(self.mbids.get(key.as_str()).map(|m| m.to_string()))
    }

    async fn similar_artists(
        &self,
        _seed_name: &str,
        seed_mbid: &str,
        limit: usize,
    ) -> Result<Vec<Neighbour>, KnowledgeError> {
        self.check("similar_artists", [seed_mbid])?;
        Ok(listed(&self.neighbours, seed_mbid, limit))
    }

    async fn artist_top_tracks(
        &self,
        artist_mbid: &str,
        limit: usize,
    ) -> Result<Vec<TrackRef>, KnowledgeError> {
        self.check("artist_top_tracks", [artist_mbid])?;
        Ok(listed(&self.top_tracks, artist_mbid, limit))
    }

    async fn artist_tags(
        &self,
        artist_mbids: &[String],
    ) -> Result<HashMap<String, Vec<WeightedTag>>, KnowledgeError> {
        self.each("artist_tags", &self.tags, artist_mbids)
    }

    async fn artist_popularity(
        &self,
        artist_mbids: &[String],
    ) -> Result<HashMap<String, u64>, KnowledgeError> {
        self.each("artist_popularity", &self.users, artist_mbids)
    }

    async fn popularity_reference(&self) -> Result<u64, KnowledgeError> {
        self.check("popularity_reference", [])?;
        Ok(self.reference)
    }

    async fn related_tags(&self, tag: &str) -> Result<Vec<String>, KnowledgeError> {
        self.check("related_tags", [tag])?;
        Ok(listed(&self.related, tag, usize::MAX)
            .into_iter()
            .map(str::to_string)
            .collect())
    }

    async fn tag_tracks(&self, tag: &str, count: usize) -> Result<Vec<TrackRef>, KnowledgeError> {
        self.check("tag_tracks", [tag])?;
        Ok(listed(&self.tag_tracks, tag, count))
    }

    async fn recordings(
        &self,
        recording_mbids: &[String],
    ) -> Result<HashMap<String, RecordingFacts>, KnowledgeError> {
        self.each("recordings", &self.recordings, recording_mbids)
    }
}

pub(crate) fn artist(name: &str, mbid: Option<&str>) -> ArtistRef {
    ArtistRef {
        name: name.to_string(),
        mbid: mbid.map(str::to_string),
    }
}

pub(crate) fn played(name: &str, mbid: Option<&str>, plays: u64) -> PlayedArtist {
    PlayedArtist {
        artist: artist(name, mbid),
        plays,
    }
}

pub(crate) fn track(
    artist_name: &str,
    artist_mbid: Option<&str>,
    title: &str,
    recording: Option<&str>,
) -> TrackRef {
    TrackRef {
        artist: artist(artist_name, artist_mbid),
        title: title.to_string(),
        mbid: recording.map(str::to_string),
    }
}

pub(crate) fn tag(name: &str, weight: f64) -> WeightedTag {
    WeightedTag {
        name: name.to_string(),
        weight,
    }
}

pub(crate) fn known_of(artist_names: &[&str]) -> KnownMusic {
    let mut known = KnownMusic::default();
    for name in artist_names {
        known.add_artist(&artist(name, None));
    }
    known
}

/// A generator that answers the same candidates and errors every run.
pub(crate) struct FixedGenerator {
    pub name: &'static str,
    pub label: &'static str,
    pub weight: f64,
    pub candidates: Vec<Candidate>,
    pub errors: SourceErrors,
}

#[async_trait]
impl CandidateGenerator for FixedGenerator {
    fn name(&self) -> &'static str {
        self.name
    }

    fn label(&self) -> &'static str {
        self.label
    }

    fn weight(&self) -> f64 {
        self.weight
    }

    async fn generate(
        &self,
        _profile: &UserMusicProfile,
        _config: &ProfileConfig,
    ) -> GeneratorOutput {
        let mut candidates = CandidateSet::new();
        for candidate in &self.candidates {
            candidates.insert(candidate.clone());
        }
        GeneratorOutput {
            candidates,
            signals: Vec::new(),
            mbid_failures: 0,
            errors: self.errors.clone(),
        }
    }
}

/// A candidate of `generator` for `title` by `artist`, under the signal `fixed`.
pub(crate) fn candidate(generator: &str, artist: &str, title: &str, score: f64) -> Candidate {
    track_candidate(track(artist, None, title, None), score, "fixed", generator)
}

pub(crate) fn fixed(
    name: &'static str,
    label: &'static str,
    weight: f64,
    tracks: &[(&str, &str, f64)],
) -> Arc<dyn CandidateGenerator> {
    Arc::new(FixedGenerator {
        name,
        label,
        weight,
        candidates: tracks
            .iter()
            .map(|(a, t, score)| candidate(name, a, t, *score))
            .collect(),
        errors: SourceErrors::default(),
    })
}

pub(crate) fn balanced() -> ProfileConfig {
    ProfileConfig::from_profile(DiscoveryProfile::Balanced)
}
