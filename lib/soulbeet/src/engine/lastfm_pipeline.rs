use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use tracing::{info, warn};

use super::ArtistCache;
use crate::error::Result;
use crate::lastfm::LastFmProvider;
use crate::traits::{CandidateGenerator, ScrobbleProvider};
use shared::recommendation::{
    Candidate, CandidateSet, CandidateSnapshot, ProfileConfig, SignalReport, TimePeriod,
    UserMusicProfile,
};

const MAX_CANDIDATES_PER_SIGNAL: usize = 500;
const WEIGHT_HOP1: f64 = 0.7;
const WEIGHT_TAG_EXPLORE: f64 = 0.5;

pub struct LastFmPipeline {
    provider: Arc<LastFmProvider>,
}

impl LastFmPipeline {
    pub fn new(provider: Arc<LastFmProvider>) -> Self {
        Self { provider }
    }

    fn known_tracks(profile: &UserMusicProfile) -> HashSet<String> {
        profile.known_track_keys.iter().cloned().collect()
    }

    fn known_artists(profile: &UserMusicProfile) -> HashSet<String> {
        profile.known_artist_names.iter().cloned().collect()
    }

    /// Truncate a candidate set to the per-signal cap.
    fn cap(set: &mut CandidateSet) {
        if set.len() > MAX_CANDIDATES_PER_SIGNAL {
            let mut entries: Vec<_> = set.candidates.drain().collect();
            entries.sort_by(|a, b| {
                b.1.score
                    .partial_cmp(&a.1.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            entries.truncate(MAX_CANDIDATES_PER_SIGNAL);
            for (k, v) in entries {
                set.candidates.insert(k, v);
            }
        }
    }

    // --- Signal 1: Track Similarity Graph ---

    async fn signal_track_graph(
        &self,
        profile: &UserMusicProfile,
        _config: &ProfileConfig,
        cache: &mut ArtistCache,
    ) -> CandidateSet {
        info!("Last.fm signal: track similarity graph");
        let mut candidates = CandidateSet::new();

        let seed_tracks = match self.provider.get_top_tracks(TimePeriod::Quarter, 20).await {
            Ok(t) => t,
            Err(e) => {
                warn!("Failed to fetch seed tracks for track graph: {}", e);
                return candidates;
            }
        };

        if seed_tracks.is_empty() {
            return candidates;
        }

        let max_play = seed_tracks
            .iter()
            .map(|t| t.play_count)
            .max()
            .unwrap_or(1)
            .max(1);
        let known = Self::known_tracks(profile);

        for seed in &seed_tracks {
            let similar = match self
                .provider
                .get_similar_tracks(&seed.artist, &seed.track, 15)
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        "Failed to get similar tracks for '{}' - '{}': {}",
                        seed.artist, seed.track, e
                    );
                    continue;
                }
            };

            let seed_weight = seed.play_count as f64 / max_play as f64;

            for sim in similar {
                let key = CandidateSet::key(&sim.artist, &sim.track);
                if known.contains(&key) {
                    continue;
                }

                let score = sim.score * seed_weight;
                let artist_listener_count = cache
                    .get_popularity(self.provider.as_ref(), &sim.artist)
                    .await;
                let primary_genre = cache.get_genre(self.provider.as_ref(), &sim.artist).await;
                candidates.insert(Candidate {
                    artist: sim.artist,
                    track: sim.track,
                    album: None,
                    mbid: sim.mbid,
                    score,
                    signals: vec!["lfm_track_graph".to_string()],
                    source: "lastfm".to_string(),
                    artist_listener_count,
                    primary_genre,
                    release_year: None,
                });
            }
        }

        Self::cap(&mut candidates);
        info!("Track graph produced {} candidates", candidates.len());
        candidates
    }

    // --- Signal 2: 2-Hop Artist Chains ---

    async fn signal_artist_chains(
        &self,
        profile: &UserMusicProfile,
        config: &ProfileConfig,
        cache: &mut ArtistCache,
    ) -> CandidateSet {
        info!("Last.fm signal: 2-hop artist chains");
        let mut candidates = CandidateSet::new();

        // Use fewer seeds and similar artists to keep API calls manageable.
        // 10 seeds x 5 hop1 = 50 similar lookups + 50 top track lookups = ~100 calls
        // Hop2: top 3 hop1 artists x 3 similar = 9 calls
        let top_artists = match self.provider.get_top_artists(TimePeriod::AllTime, 10).await {
            Ok(a) => a,
            Err(e) => {
                warn!("Failed to fetch top artists for artist chains: {}", e);
                return candidates;
            }
        };

        let known_artists = Self::known_artists(profile);

        for seed in &top_artists {
            let hop1_artists = match self.provider.get_similar_artists(&seed.name, 5).await {
                Ok(a) => a,
                Err(e) => {
                    warn!("Failed to get similar artists for '{}': {}", seed.name, e);
                    continue;
                }
            };

            for hop1 in &hop1_artists {
                // Hop 1: get tracks for unknown artists
                if !known_artists.contains(&hop1.name.to_lowercase()) {
                    let tracks = match self.provider.get_artist_top_tracks(&hop1.name, 3).await {
                        Ok(t) => t,
                        Err(e) => {
                            warn!("Failed to get top tracks for hop1 '{}': {}", hop1.name, e);
                            continue;
                        }
                    };

                    let score = hop1.score * WEIGHT_HOP1;
                    let artist_listener_count = cache
                        .get_popularity(self.provider.as_ref(), &hop1.name)
                        .await;
                    let primary_genre = cache.get_genre(self.provider.as_ref(), &hop1.name).await;
                    for track in tracks {
                        candidates.insert(Candidate {
                            artist: track.artist.clone(),
                            track: track.track.clone(),
                            album: None,
                            mbid: track.mbid.clone(),
                            score,
                            signals: vec!["lfm_artist_hop1".to_string()],
                            source: "lastfm".to_string(),
                            artist_listener_count,
                            primary_genre: primary_genre.clone(),
                            release_year: None,
                        });
                    }
                }

                // Hop 2 - only for top 3 hop1 artists per seed to limit API calls
                if hop1_artists
                    .iter()
                    .position(|a| a.name == hop1.name)
                    .unwrap_or(99)
                    >= 3
                {
                    continue;
                }
                let hop2_artists = match self.provider.get_similar_artists(&hop1.name, 3).await {
                    Ok(a) => a,
                    Err(e) => {
                        warn!(
                            "Failed to get hop2 similar artists for '{}': {}",
                            hop1.name, e
                        );
                        continue;
                    }
                };

                for hop2 in &hop2_artists {
                    if known_artists.contains(&hop2.name.to_lowercase())
                        || hop2.name.to_lowercase() == seed.name.to_lowercase()
                    {
                        continue;
                    }

                    let tracks = match self.provider.get_artist_top_tracks(&hop2.name, 3).await {
                        Ok(t) => t,
                        Err(e) => {
                            warn!("Failed to get top tracks for hop2 '{}': {}", hop2.name, e);
                            continue;
                        }
                    };

                    let score = hop1.score * hop2.score * config.hop2_weight;
                    let artist_listener_count = cache
                        .get_popularity(self.provider.as_ref(), &hop2.name)
                        .await;
                    let primary_genre = cache.get_genre(self.provider.as_ref(), &hop2.name).await;
                    for track in tracks {
                        candidates.insert(Candidate {
                            artist: track.artist.clone(),
                            track: track.track.clone(),
                            album: None,
                            mbid: track.mbid.clone(),
                            score,
                            signals: vec!["lfm_artist_hop2".to_string()],
                            source: "lastfm".to_string(),
                            artist_listener_count,
                            primary_genre: primary_genre.clone(),
                            release_year: None,
                        });
                    }
                }
            }
        }

        Self::cap(&mut candidates);
        info!("Artist chains produced {} candidates", candidates.len());
        candidates
    }

    // --- Signal 3: Tag-Based Discovery ---

    async fn signal_tag_explore(
        &self,
        profile: &UserMusicProfile,
        _config: &ProfileConfig,
        cache: &mut ArtistCache,
    ) -> CandidateSet {
        info!("Last.fm signal: tag-based exploration");
        let mut candidates = CandidateSet::new();

        if profile.tag_exploration_zone.is_empty() {
            warn!("No exploration tags in profile, skipping tag explore signal");
            return candidates;
        }

        // Build a weight lookup from the genre distribution
        let tag_weight_map: std::collections::HashMap<String, f64> = profile
            .genre_distribution
            .iter()
            .map(|t| (t.name.to_lowercase(), t.weight))
            .collect();

        let known = Self::known_tracks(profile);

        // Cap at 5 tags to limit API calls
        for tag in profile.tag_exploration_zone.iter().take(5) {
            let tracks = match self.provider.get_tag_top_tracks(tag, 20).await {
                Ok(t) => t,
                Err(e) => {
                    warn!("Failed to get tag top tracks for '{}': {}", tag, e);
                    continue;
                }
            };

            let tag_w = tag_weight_map
                .get(&tag.to_lowercase())
                .copied()
                .unwrap_or(0.1);

            for track in tracks {
                let key = CandidateSet::key(&track.artist, &track.track);
                if known.contains(&key) {
                    continue;
                }

                // Floor at 0.15 so exploration candidates can compete in greedy
                // selection instead of relying entirely on backfill
                let score = (WEIGHT_TAG_EXPLORE * tag_w).max(0.15);
                let artist_listener_count = cache
                    .get_popularity(self.provider.as_ref(), &track.artist)
                    .await;
                let primary_genre = cache.get_genre(self.provider.as_ref(), &track.artist).await;
                candidates.insert(Candidate {
                    artist: track.artist,
                    track: track.track,
                    album: None,
                    mbid: track.mbid,
                    score,
                    signals: vec!["lfm_tag_explore".to_string()],
                    source: "lastfm".to_string(),
                    artist_listener_count,
                    primary_genre,
                    release_year: None,
                });
            }
        }

        Self::cap(&mut candidates);
        info!("Tag explore produced {} candidates", candidates.len());
        candidates
    }

    // --- Signal 4: Temporal Drift / Momentum ---

    async fn signal_momentum(
        &self,
        profile: &UserMusicProfile,
        config: &ProfileConfig,
        cache: &mut ArtistCache,
    ) -> CandidateSet {
        info!("Last.fm signal: momentum artists");
        let mut candidates = CandidateSet::new();

        if profile.momentum_artists.is_empty() {
            warn!("No momentum artists in profile, skipping momentum signal");
            return candidates;
        }

        let known_artists = Self::known_artists(profile);

        for ma in &profile.momentum_artists {
            let similar = match self.provider.get_similar_artists(&ma.name, 10).await {
                Ok(a) => a,
                Err(e) => {
                    warn!(
                        "Failed to get similar artists for momentum artist '{}': {}",
                        ma.name, e
                    );
                    continue;
                }
            };

            for sim in similar {
                if known_artists.contains(&sim.name.to_lowercase()) {
                    continue;
                }

                let tracks = match self.provider.get_artist_top_tracks(&sim.name, 3).await {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(
                            "Failed to get top tracks for momentum-similar '{}': {}",
                            sim.name, e
                        );
                        continue;
                    }
                };

                let score = ma.momentum_score * sim.score * config.momentum_weight;
                let artist_listener_count = cache
                    .get_popularity(self.provider.as_ref(), &sim.name)
                    .await;
                let primary_genre = cache.get_genre(self.provider.as_ref(), &sim.name).await;
                for track in tracks {
                    candidates.insert(Candidate {
                        artist: track.artist,
                        track: track.track,
                        album: None,
                        mbid: track.mbid,
                        score,
                        signals: vec!["lfm_momentum".to_string()],
                        source: "lastfm".to_string(),
                        artist_listener_count,
                        primary_genre: primary_genre.clone(),
                        release_year: None,
                    });
                }
            }
        }

        Self::cap(&mut candidates);
        info!("Momentum signal produced {} candidates", candidates.len());
        candidates
    }
}

#[async_trait]
impl CandidateGenerator for LastFmPipeline {
    fn name(&self) -> &str {
        "lastfm_pipeline"
    }

    async fn generate_candidates(
        &self,
        profile: &UserMusicProfile,
        config: &ProfileConfig,
    ) -> Result<(CandidateSet, Vec<SignalReport>, usize)> {
        info!("Running Last.fm candidate generation pipeline");
        let mut combined = CandidateSet::new();
        let mut cache = ArtistCache::new();
        let mut signal_reports = Vec::new();

        // Run each signal, merging results. Each signal is independent;
        // if one fails the others still contribute.
        // The cache accumulates across signals within this pipeline run.
        let track_graph = self.signal_track_graph(profile, config, &mut cache).await;
        signal_reports.push(build_signal_report("lfm_track_graph", &track_graph));
        for (_, c) in track_graph.candidates {
            combined.insert(c);
        }

        let chains = self.signal_artist_chains(profile, config, &mut cache).await;
        signal_reports.push(build_signal_report("lfm_artist_chains", &chains));
        for (_, c) in chains.candidates {
            combined.insert(c);
        }

        let tag_explore = self.signal_tag_explore(profile, config, &mut cache).await;
        signal_reports.push(build_signal_report("lfm_tag_explore", &tag_explore));
        for (_, c) in tag_explore.candidates {
            combined.insert(c);
        }

        let momentum = self.signal_momentum(profile, config, &mut cache).await;
        signal_reports.push(build_signal_report("lfm_momentum", &momentum));
        for (_, c) in momentum.candidates {
            combined.insert(c);
        }

        info!(
            "Last.fm pipeline total: {} unique candidates ({} artists cached)",
            combined.len(),
            cache.popularity.len() + cache.genre.len(),
        );
        Ok((combined, signal_reports, 0))
    }
}

fn build_signal_report(name: &str, set: &CandidateSet) -> SignalReport {
    let mut sorted: Vec<&Candidate> = set.candidates.values().collect();
    sorted.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    SignalReport {
        name: name.to_string(),
        candidates_produced: set.len(),
        top_candidates: sorted
            .iter()
            .take(3)
            .map(|c| CandidateSnapshot::from_candidate(c))
            .collect(),
    }
}
