pub mod blender;
pub mod diversifier;
pub mod freshness;
pub mod lastfm_pipeline;
pub mod listenbrainz_pipeline;
pub mod profile;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tracing::{info, warn};

use crate::error::Result;
use crate::traits::{CandidateGenerator, ScrobbleProvider};
use shared::recommendation::{
    Candidate, CandidateSet, EngineReport, PipelineReport, ProfileConfig, ProfileSummary,
    TimePeriod, UserMusicProfile,
};

const ARTIST_CACHE_LIMIT: usize = 100;

/// Per-pipeline-run cache for artist popularity and genre metadata.
/// Avoids redundant API calls when the same artist appears across signals.
/// Stops fetching new metadata after encountering `ARTIST_CACHE_LIMIT` unique artists.
pub(crate) struct ArtistCache {
    pub popularity: HashMap<String, u64>,
    pub genre: HashMap<String, String>,
    popularity_fetches: usize,
    genre_fetches: usize,
}

impl ArtistCache {
    pub fn new() -> Self {
        Self {
            popularity: HashMap::new(),
            genre: HashMap::new(),
            popularity_fetches: 0,
            genre_fetches: 0,
        }
    }

    pub async fn get_popularity(
        &mut self,
        provider: &dyn ScrobbleProvider,
        artist: &str,
    ) -> Option<u64> {
        let key = artist.to_lowercase();
        if let Some(&count) = self.popularity.get(&key) {
            return Some(count);
        }
        if self.popularity_fetches >= ARTIST_CACHE_LIMIT {
            return None;
        }
        if let Ok(pop) = provider.get_artist_popularity(artist).await {
            self.popularity_fetches += 1;
            self.popularity.insert(key, pop.listener_count);
            return Some(pop.listener_count);
        }
        None
    }

    pub async fn get_genre(
        &mut self,
        provider: &dyn ScrobbleProvider,
        artist: &str,
    ) -> Option<String> {
        let key = artist.to_lowercase();
        if let Some(genre) = self.genre.get(&key) {
            return Some(genre.clone());
        }
        if self.genre_fetches >= ARTIST_CACHE_LIMIT {
            return None;
        }
        if let Ok(tags) = provider.get_artist_tags(artist).await {
            self.genre_fetches += 1;
            if let Some(top) = tags.first() {
                self.genre.insert(key, top.name.clone());
                return Some(top.name.clone());
            }
        }
        None
    }
}

pub use lastfm_pipeline::LastFmPipeline;
pub use listenbrainz_pipeline::ListenBrainzPipeline;
pub use profile::build_profile;

/// Run the full recommendation pipeline with a pre-built profile.
///
/// 1. Generate candidates from each generator.
/// 2. Blend results across sources.
/// 3. Apply freshness penalties.
/// 4. Diversify and select the final list.
///
/// Returns the final candidates alongside a structured report of all decisions.
pub async fn recommend(
    providers: &[Arc<dyn ScrobbleProvider>],
    generators: &[Arc<dyn CandidateGenerator>],
    profile: &UserMusicProfile,
    config: &ProfileConfig,
    target_count: usize,
) -> Result<(Vec<Candidate>, EngineReport)> {
    let start = std::time::Instant::now();

    info!(
        "Starting recommendation: {} generators, target {}",
        generators.len(),
        target_count
    );

    let mut report = EngineReport {
        profile_summary: ProfileSummary {
            genre_count: profile.genre_distribution.len(),
            top_genres: profile
                .genre_distribution
                .iter()
                .take(5)
                .map(|t| t.name.clone())
                .collect(),
            obscurity_score: profile.obscurity_score,
            repeat_ratio: profile.repeat_ratio,
            freshness_half_life_days: profile.freshness_half_life_days,
            momentum_artists: profile
                .momentum_artists
                .iter()
                .map(|a| a.name.clone())
                .collect(),
            comfort_tags: profile.tag_comfort_zone.len(),
            exploration_tags: profile.tag_exploration_zone.len(),
        },
        ..Default::default()
    };

    // Step 1: Generate candidates from each pipeline.
    let mut source_sets: Vec<(&str, CandidateSet)> = Vec::new();

    for generator in generators {
        // Per-pipeline timeout: 2 minutes max
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3600),
            generator.generate_candidates(profile, config),
        )
        .await;

        let result = match result {
            Ok(r) => r,
            Err(_) => {
                warn!("Generator '{}' timed out after 1h", generator.name());
                report.pipeline_reports.push(PipelineReport {
                    name: format!("{} (TIMEOUT)", generator.name()),
                    signals: vec![],
                    total_candidates: 0,
                    mbid_failures: 0,
                });
                continue;
            }
        };

        match result {
            Ok((set, signal_reports, mbid_failures)) => {
                info!(
                    "Generator '{}' produced {} candidates",
                    generator.name(),
                    set.len()
                );
                report.pipeline_reports.push(PipelineReport {
                    name: generator.name().to_string(),
                    signals: signal_reports,
                    total_candidates: set.len(),
                    mbid_failures,
                });
                source_sets.push((generator.name(), set));
            }
            Err(e) => {
                warn!("Generator '{}' failed: {}", generator.name(), e);
                report.pipeline_reports.push(PipelineReport {
                    name: format!("{} (ERROR: {})", generator.name(), e),
                    signals: vec![],
                    total_candidates: 0,
                    mbid_failures: 0,
                });
            }
        }
    }

    if source_sets.is_empty() {
        warn!("No generators produced candidates");
        report.duration_secs = start.elapsed().as_secs_f64();
        return Ok((vec![], report));
    }

    // Step 2: Blend
    let (mut blended, blend_summary) = blender::blend(source_sets, config);
    info!("After blending: {} candidates", blended.len());
    report.blend_summary = blend_summary;

    // Step 2.5: Enrich top candidates with release year from MusicBrainz
    enrich_release_years(&mut blended, target_count * 3).await;

    // Step 3: Freshness
    let known_artists = collect_known_artists(providers).await;
    let freshness_summary =
        freshness::apply_freshness(&mut blended, profile, &known_artists, config);
    report.freshness_summary = freshness_summary;

    // Step 4: Diversify
    let (result, diversifier_summary) =
        diversifier::diversify(blended, profile, config, target_count);
    info!("Final recommendation: {} tracks", result.len());
    report.diversifier_summary = diversifier_summary;

    report.final_count = result.len();
    report.duration_secs = start.elapsed().as_secs_f64();

    Ok((result, report))
}

/// Build a user profile by merging data from all available providers, then run the pipeline.
///
/// Uses the first provider with meaningful data as the primary source, then
/// enriches the profile with data from additional providers (extra genres,
/// momentum artists, known artists/tracks).
pub async fn build_and_recommend(
    providers: &[Arc<dyn ScrobbleProvider>],
    generators: &[Arc<dyn CandidateGenerator>],
    _profile_provider: &dyn ScrobbleProvider,
    config: &ProfileConfig,
    target_count: usize,
) -> Result<(UserMusicProfile, Vec<Candidate>, EngineReport)> {
    let mut profile = UserMusicProfile::default();
    let mut profile_sources: Vec<String> = Vec::new();

    for provider in providers {
        info!("Trying profile build from {}", provider.name());
        match build_profile(provider.as_ref()).await {
            Ok(p) if !p.genre_distribution.is_empty() || !p.momentum_artists.is_empty() => {
                info!(
                    "Profile built from {} with {} genres, {} momentum artists",
                    provider.name(),
                    p.genre_distribution.len(),
                    p.momentum_artists.len()
                );
                if profile_sources.is_empty() {
                    // First provider becomes the base profile
                    profile = p;
                } else {
                    // Merge additional provider data into existing profile
                    merge_profile(&mut profile, &p);
                }
                profile_sources.push(provider.name().to_string());
            }
            Ok(_) => {
                info!("{} returned empty profile, skipping", provider.name());
            }
            Err(e) => {
                warn!(
                    "Profile build from {} failed: {}, skipping",
                    provider.name(),
                    e
                );
            }
        }
    }

    let profile_source = if profile_sources.is_empty() {
        "none".to_string()
    } else {
        profile_sources.join("+")
    };

    let (recommendations, mut report) =
        recommend(providers, generators, &profile, config, target_count).await?;
    report.profile_source = profile_source;
    Ok((profile, recommendations, report))
}

/// Merge a secondary profile into the primary, adding new data without replacing.
fn merge_profile(primary: &mut UserMusicProfile, secondary: &UserMusicProfile) {
    // Merge genre distribution: add new genres, boost existing ones
    let mut genre_map: HashMap<String, f64> = primary
        .genre_distribution
        .iter()
        .map(|t| (t.name.clone(), t.weight))
        .collect();
    for tag in &secondary.genre_distribution {
        let entry = genre_map.entry(tag.name.clone()).or_default();
        *entry += tag.weight * 0.5; // secondary provider contributes at half weight
    }
    // Re-normalize
    let total: f64 = genre_map.values().sum();
    if total > 0.0 {
        let mut tags: Vec<_> = genre_map
            .into_iter()
            .map(|(name, w)| shared::recommendation::WeightedTag {
                name,
                weight: w / total,
            })
            .collect();
        tags.sort_by(|a, b| {
            b.weight
                .partial_cmp(&a.weight)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        primary.genre_distribution = tags;
    }

    // Merge momentum artists: add new ones not already present
    let existing: HashSet<String> = primary
        .momentum_artists
        .iter()
        .map(|a| a.name.to_lowercase())
        .collect();
    for ma in &secondary.momentum_artists {
        if !existing.contains(&ma.name.to_lowercase()) {
            primary.momentum_artists.push(ma.clone());
        }
    }
    primary.momentum_artists.truncate(15);

    // Merge known artists/tracks (union)
    let mut known_artists: HashSet<String> = primary.known_artist_names.iter().cloned().collect();
    known_artists.extend(secondary.known_artist_names.iter().cloned());
    primary.known_artist_names = known_artists.into_iter().collect();

    let mut known_tracks: HashSet<String> = primary.known_track_keys.iter().cloned().collect();
    known_tracks.extend(secondary.known_track_keys.iter().cloned());
    primary.known_track_keys = known_tracks.into_iter().collect();
}

/// Collect a set of known artist names (lowercased) from all providers.
async fn collect_known_artists(providers: &[Arc<dyn ScrobbleProvider>]) -> HashSet<String> {
    let mut known = HashSet::new();
    for provider in providers {
        match provider.get_top_artists(TimePeriod::AllTime, 200).await {
            Ok(artists) => {
                for a in artists {
                    known.insert(a.name.to_lowercase());
                }
            }
            Err(e) => {
                warn!(
                    "Failed to fetch known artists from {}: {}",
                    provider.name(),
                    e
                );
            }
        }
    }
    known
}

/// Enrich the top candidates with release year from MusicBrainz recording lookups.
/// Only looks up candidates that have a recording MBID and no release_year yet.
/// Limited to `max_lookups` to keep API calls bounded.
async fn enrich_release_years(candidates: &mut CandidateSet, max_lookups: usize) {
    let client = crate::http::build_client("soulful/0.1 (https://github.com/soulful)");

    // Sort by score descending and only enrich the top candidates
    let mut by_score: Vec<String> = candidates.candidates.keys().cloned().collect();
    by_score.sort_by(|a, b| {
        let sa = candidates.candidates.get(a).map(|c| c.score).unwrap_or(0.0);
        let sb = candidates.candidates.get(b).map(|c| c.score).unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut lookups = 0usize;
    for key in by_score {
        if lookups >= max_lookups {
            break;
        }
        let candidate = match candidates.candidates.get(&key) {
            Some(c) => c,
            None => continue,
        };
        if candidate.release_year.is_some() {
            continue;
        }
        let mbid = match &candidate.mbid {
            Some(id) if !id.is_empty() => id.clone(),
            _ => continue,
        };

        lookups += 1;
        if let Ok(Some(info)) = crate::http::cached_recording_lookup(&client, &mbid).await {
            if let Some(year) = info.release_year {
                if let Some(c) = candidates.candidates.get_mut(&key) {
                    c.release_year = Some(year);
                }
            }
        }
    }

    if lookups > 0 {
        let filled = candidates
            .candidates
            .values()
            .filter(|c| c.release_year.is_some())
            .count();
        info!(
            "Release year enrichment: {} lookups, {} candidates now have years",
            lookups, filled
        );
    }
}
