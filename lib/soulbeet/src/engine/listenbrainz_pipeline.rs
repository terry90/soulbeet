use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use tracing::{info, warn};

use super::ArtistCache;
use crate::error::Result;
use crate::listenbrainz::ListenBrainzProvider;
use crate::traits::{CandidateGenerator, ScrobbleProvider};
use shared::recommendation::{
    Candidate, CandidateSet, CandidateSnapshot, ProfileConfig, SignalReport, TimePeriod,
    UserMusicProfile,
};

const MAX_CANDIDATES_PER_SIGNAL: usize = 500;
const WEIGHT_LB_NATIVE: f64 = 1.0;
const WEIGHT_ARTIST_SIM: f64 = 0.7;
const WEIGHT_GENRE_EXPLORE: f64 = 0.5;

pub struct ListenBrainzPipeline {
    provider: Arc<ListenBrainzProvider>,
}

impl ListenBrainzPipeline {
    pub fn new(provider: Arc<ListenBrainzProvider>) -> Self {
        Self { provider }
    }

    fn known_artists(profile: &UserMusicProfile) -> HashSet<String> {
        profile.known_artist_names.iter().cloned().collect()
    }

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

    // --- Signal 1: Collaborative Filtering (Similar Users) ---

    async fn signal_collab_filter(
        &self,
        _profile: &UserMusicProfile,
        _config: &ProfileConfig,
        cache: &mut ArtistCache,
    ) -> CandidateSet {
        info!("ListenBrainz signal: collaborative filtering");
        let mut candidates = CandidateSet::new();

        let similar_users = match self.provider.client().get_similar_users().await {
            Ok(users) => users,
            Err(e) => {
                warn!("Failed to fetch similar users: {}", e);
                return candidates;
            }
        };

        // Take top 10 similar users to limit API calls
        let users: Vec<_> = similar_users.into_iter().take(10).collect();

        // Fetch known user tracks to filter
        let own_tracks = match self.provider.get_top_tracks(TimePeriod::AllTime, 200).await {
            Ok(t) => t,
            Err(e) => {
                warn!("Failed to fetch own tracks for collab filter: {}", e);
                return candidates;
            }
        };
        let own_track_keys: HashSet<String> = own_tracks
            .iter()
            .map(|t| CandidateSet::key(&t.artist, &t.track))
            .collect();

        for user in &users {
            let recordings = match self
                .provider
                .client()
                .get_user_top_recordings(&user.user_name, TimePeriod::Quarter, 50)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    warn!(
                        "Failed to fetch recordings for similar user '{}': {}",
                        user.user_name, e
                    );
                    continue;
                }
            };

            for rec in recordings.payload.recordings {
                let key = CandidateSet::key(&rec.artist_name, &rec.track_name);
                if own_track_keys.contains(&key) {
                    continue;
                }

                let play_score = if rec.listen_count > 0 {
                    (rec.listen_count as f64).ln()
                } else {
                    0.1
                };
                let score = user.similarity * play_score;

                let artist_listener_count = cache
                    .get_popularity(self.provider.as_ref(), &rec.artist_name)
                    .await;
                let primary_genre = cache
                    .get_genre(self.provider.as_ref(), &rec.artist_name)
                    .await;
                candidates.insert(Candidate {
                    artist: rec.artist_name,
                    track: rec.track_name,
                    album: None,
                    mbid: rec.recording_mbid,
                    score,
                    signals: vec!["lb_collab_filter".to_string()],
                    source: "listenbrainz".to_string(),
                    artist_listener_count,
                    primary_genre,
                    release_year: None,
                });
            }
        }

        Self::cap(&mut candidates);
        info!("Collab filter produced {} candidates", candidates.len());
        candidates
    }

    // --- Signal 2: Troi Recommendations ---

    async fn signal_troi(
        &self,
        _profile: &UserMusicProfile,
        _config: &ProfileConfig,
        cache: &mut ArtistCache,
    ) -> CandidateSet {
        info!("ListenBrainz signal: Troi recommendations");
        let mut candidates = CandidateSet::new();

        let playlists = match self.provider.client().get_recommendation_playlists().await {
            Ok(r) => r,
            Err(e) => {
                warn!("Failed to fetch recommendation playlists: {}", e);
                return candidates;
            }
        };

        // Collect tracks from all recommendation playlists
        for pw in playlists.playlists {
            let total = pw.playlist.track.len().max(1) as f64;
            for (i, track) in pw.playlist.track.into_iter().enumerate() {
                // Position-based decay: 1.0 down to 0.5
                let pos_factor = 1.0 - 0.5 * (i as f64 / total);
                let score = WEIGHT_LB_NATIVE * pos_factor;

                let artist_listener_count = cache
                    .get_popularity(self.provider.as_ref(), &track.creator)
                    .await;
                let primary_genre = cache
                    .get_genre(self.provider.as_ref(), &track.creator)
                    .await;
                candidates.insert(Candidate {
                    artist: track.creator,
                    track: track.title,
                    album: None,
                    mbid: track.identifier,
                    score,
                    signals: vec!["lb_troi".to_string()],
                    source: "listenbrainz".to_string(),
                    artist_listener_count,
                    primary_genre,
                    release_year: None,
                });
            }
        }

        Self::cap(&mut candidates);
        info!("Troi signal produced {} candidates", candidates.len());
        candidates
    }

    // --- Signal 3: Artist Radio Expansion ---

    async fn signal_artist_radio(
        &self,
        profile: &UserMusicProfile,
        _config: &ProfileConfig,
        cache: &mut ArtistCache,
    ) -> CandidateSet {
        info!("ListenBrainz signal: artist radio expansion");
        let mut candidates = CandidateSet::new();

        // Use top 10 artists to limit API calls (each triggers an MBID lookup + radio call)
        let top_artists = match self.provider.get_top_artists(TimePeriod::AllTime, 10).await {
            Ok(a) => a,
            Err(e) => {
                warn!("Failed to fetch top artists for artist radio: {}", e);
                return candidates;
            }
        };

        let known_artists = Self::known_artists(profile);

        for artist in &top_artists {
            let similar = match self.provider.get_similar_artists(&artist.name, 5).await {
                Ok(a) => a,
                Err(e) => {
                    warn!("Failed to get similar artists for '{}': {}", artist.name, e);
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
                            "Failed to get top tracks for similar artist '{}': {}",
                            sim.name, e
                        );
                        continue;
                    }
                };

                let score = sim.score * WEIGHT_ARTIST_SIM;
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
                        signals: vec!["lb_artist_radio".to_string()],
                        source: "listenbrainz".to_string(),
                        artist_listener_count,
                        primary_genre: primary_genre.clone(),
                        release_year: None,
                    });
                }
            }
        }

        Self::cap(&mut candidates);
        info!("Artist radio produced {} candidates", candidates.len());
        candidates
    }

    // --- Signal 4: Tag/Genre Exploration ---

    async fn signal_genre_explore(
        &self,
        profile: &UserMusicProfile,
        _config: &ProfileConfig,
        cache: &mut ArtistCache,
    ) -> CandidateSet {
        info!("ListenBrainz signal: genre exploration");
        let mut candidates = CandidateSet::new();

        if profile.tag_exploration_zone.is_empty() {
            warn!("No exploration tags in profile, skipping genre explore signal");
            return candidates;
        }

        // Cap at 5 tags to limit API calls
        for tag in profile.tag_exploration_zone.iter().take(5) {
            let tracks = match self.provider.get_tag_top_tracks(tag, 20).await {
                Ok(t) => t,
                Err(e) => {
                    warn!("Failed to get tag top tracks for '{}': {}", tag, e);
                    continue;
                }
            };

            for track in tracks {
                if track.artist.is_empty() {
                    continue; // skip entries without artist info
                }
                let artist_listener_count = cache
                    .get_popularity(self.provider.as_ref(), &track.artist)
                    .await;
                let primary_genre = cache.get_genre(self.provider.as_ref(), &track.artist).await;
                // Floor at 0.15 so exploration candidates can compete in greedy
                // selection instead of relying entirely on backfill
                let score = WEIGHT_GENRE_EXPLORE.max(0.15);
                candidates.insert(Candidate {
                    artist: track.artist,
                    track: track.track,
                    album: None,
                    mbid: track.mbid,
                    score,
                    signals: vec!["lb_genre_explore".to_string()],
                    source: "listenbrainz".to_string(),
                    artist_listener_count,
                    primary_genre,
                    release_year: None,
                });
            }
        }

        Self::cap(&mut candidates);
        info!("Genre explore produced {} candidates", candidates.len());
        candidates
    }
}

#[async_trait]
impl CandidateGenerator for ListenBrainzPipeline {
    fn name(&self) -> &str {
        "listenbrainz_pipeline"
    }

    async fn generate_candidates(
        &self,
        profile: &UserMusicProfile,
        config: &ProfileConfig,
    ) -> Result<(CandidateSet, Vec<SignalReport>, usize)> {
        info!("Running ListenBrainz candidate generation pipeline");
        let mut combined = CandidateSet::new();
        let mut cache = ArtistCache::new();
        let mut signal_reports = Vec::new();

        // Discard any stale MBID failure count from prior runs or profile building
        self.provider.take_mbid_failures();

        // The cache accumulates across signals within this pipeline run.
        let collab = self.signal_collab_filter(profile, config, &mut cache).await;
        signal_reports.push(build_signal_report("lb_collab_filter", &collab));
        for (_, c) in collab.candidates {
            combined.insert(c);
        }

        let troi = self.signal_troi(profile, config, &mut cache).await;
        signal_reports.push(build_signal_report("lb_troi", &troi));
        for (_, c) in troi.candidates {
            combined.insert(c);
        }

        let radio = self.signal_artist_radio(profile, config, &mut cache).await;
        signal_reports.push(build_signal_report("lb_artist_radio", &radio));
        for (_, c) in radio.candidates {
            combined.insert(c);
        }

        let genre = self.signal_genre_explore(profile, config, &mut cache).await;
        signal_reports.push(build_signal_report("lb_genre_explore", &genre));
        for (_, c) in genre.candidates {
            combined.insert(c);
        }

        let mbid_failures = self.provider.take_mbid_failures();
        info!(
            "ListenBrainz pipeline total: {} unique candidates ({} artists cached, {} MBID failures)",
            combined.len(),
            cache.popularity.len() + cache.genre.len(),
            mbid_failures,
        );
        Ok((combined, signal_reports, mbid_failures))
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
