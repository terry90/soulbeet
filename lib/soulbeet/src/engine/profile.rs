use std::collections::{HashMap, HashSet};

use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::error::Result;
use crate::traits::ScrobbleProvider;
use shared::recommendation::{MomentumArtist, TimePeriod, UserMusicProfile, WeightedTag};

/// Build a full user music profile from scrobble data.
///
/// Each step is independent. If a step fails, its field falls back to a
/// default value and the rest of the profile still gets built.
pub async fn build_profile(provider: &dyn ScrobbleProvider) -> Result<UserMusicProfile> {
    info!("Building user music profile from {}", provider.name());

    let mut profile = UserMusicProfile::default();

    // Fetch the two datasets many steps depend on.
    let mut top_artists_alltime = provider
        .get_top_artists(TimePeriod::AllTime, 200)
        .await
        .unwrap_or_default();

    let mut top_tracks_alltime = provider
        .get_top_tracks(TimePeriod::AllTime, 200)
        .await
        .unwrap_or_default();

    // If stats are empty (e.g. ListenBrainz hasn't computed them yet),
    // build them from raw listens.
    if top_artists_alltime.is_empty() || top_tracks_alltime.is_empty() {
        info!("Stats empty, building from raw listens");
        let listens = provider.get_listens(1000).await.unwrap_or_default();
        if !listens.is_empty() {
            if top_artists_alltime.is_empty() {
                let mut artist_counts: HashMap<String, u64> = HashMap::new();
                for l in &listens {
                    *artist_counts.entry(l.artist.clone()).or_default() += 1;
                }
                let mut artists: Vec<_> = artist_counts.into_iter().collect();
                artists.sort_by_key(|a| std::cmp::Reverse(a.1));
                top_artists_alltime = artists
                    .into_iter()
                    .map(|(name, play_count)| shared::recommendation::RankedArtist {
                        name,
                        mbid: None,
                        play_count,
                    })
                    .collect();
                info!(
                    "Built {} artists from raw listens",
                    top_artists_alltime.len()
                );
            }
            if top_tracks_alltime.is_empty() {
                let mut track_counts: HashMap<(String, String), u64> = HashMap::new();
                for l in &listens {
                    *track_counts
                        .entry((l.artist.clone(), l.track.clone()))
                        .or_default() += 1;
                }
                let mut tracks: Vec<_> = track_counts.into_iter().collect();
                tracks.sort_by_key(|a| std::cmp::Reverse(a.1));
                top_tracks_alltime = tracks
                    .into_iter()
                    .map(
                        |((artist, track), play_count)| shared::recommendation::RankedTrack {
                            artist,
                            track,
                            mbid: None,
                            play_count,
                        },
                    )
                    .collect();
                info!("Built {} tracks from raw listens", top_tracks_alltime.len());
            }
        }
    }

    // --- Step 1: Genre distribution ---
    match build_genre_distribution(provider, &top_artists_alltime).await {
        Ok(tags) => profile.genre_distribution = tags,
        Err(e) => warn!("Genre distribution step failed: {}", e),
    }

    // --- Step 2: Obscurity score ---
    match build_obscurity_score(provider, &top_artists_alltime).await {
        Ok(score) => profile.obscurity_score = score,
        Err(e) => warn!("Obscurity score step failed: {}", e),
    }

    // --- Step 3: Repeat ratio (from raw listens, not deduplicated top tracks) ---
    {
        let listens = provider.get_listens(1000).await.unwrap_or_default();
        if !listens.is_empty() {
            let unique: HashSet<String> = listens
                .iter()
                .map(|l| format!("{}:{}", l.artist.to_lowercase(), l.track.to_lowercase()))
                .collect();
            profile.repeat_ratio = unique.len() as f64 / listens.len() as f64;
        }
    }

    // --- Step 4: Freshness half-life ---
    match build_freshness_half_life(provider).await {
        Ok(hl) => profile.freshness_half_life_days = hl,
        Err(e) => {
            warn!("Freshness half-life step failed: {}", e);
            profile.freshness_half_life_days = 90.0; // default
        }
    }

    // --- Step 5: Momentum artists ---
    match build_momentum_artists(provider, &top_artists_alltime).await {
        Ok(artists) => profile.momentum_artists = artists,
        Err(e) => warn!("Momentum artists step failed: {}", e),
    }

    // --- Step 6: Era preference ---
    // Skipped -- requires release dates that are expensive to fetch.
    // Will be filled later from Navidrome data.
    profile.era_preference = vec![];

    // --- Step 7: Tag zones ---
    build_tag_zones(&mut profile, provider).await;

    // --- Step 8: Known artists and tracks for pipeline filtering ---
    profile.known_artist_names = top_artists_alltime
        .iter()
        .map(|a| a.name.to_lowercase())
        .collect();
    profile.known_track_keys = top_tracks_alltime
        .iter()
        .map(|t| format!("{}:{}", t.artist.to_lowercase(), t.track.to_lowercase()))
        .collect();

    // --- Step 9: Top artists hash ---
    let top_names: Vec<String> = top_artists_alltime
        .iter()
        .take(20)
        .map(|a| a.name.clone())
        .collect();
    let joined = top_names.join("|");
    let mut hasher = Sha256::new();
    hasher.update(joined.as_bytes());
    profile.top_artists_hash = format!("{:x}", hasher.finalize());

    info!(
        "Profile built: {} genres, obscurity={:.2}, repeat_ratio={:.2}, half_life={:.0}d, {} momentum artists",
        profile.genre_distribution.len(),
        profile.obscurity_score,
        profile.repeat_ratio,
        profile.freshness_half_life_days,
        profile.momentum_artists.len(),
    );

    Ok(profile)
}

/// Step 1: Accumulate weighted tags from top artists.
async fn build_genre_distribution(
    provider: &dyn ScrobbleProvider,
    top_artists: &[shared::recommendation::RankedArtist],
) -> Result<Vec<WeightedTag>> {
    let artists_to_query = top_artists.iter().take(50);
    let mut tag_weights: HashMap<String, f64> = HashMap::new();

    for artist in artists_to_query {
        let tags = match provider.get_artist_tags(&artist.name).await {
            Ok(t) => t,
            Err(e) => {
                warn!("Failed to get tags for '{}': {}", artist.name, e);
                continue;
            }
        };
        for tag in tags {
            let key = tag.name.to_lowercase();
            *tag_weights.entry(key).or_default() += tag.weight * artist.play_count as f64;
        }
    }

    if tag_weights.is_empty() {
        return Ok(vec![]);
    }

    // Normalize so weights sum to 1.0
    let total: f64 = tag_weights.values().sum();
    let mut tags: Vec<WeightedTag> = tag_weights
        .into_iter()
        .map(|(name, w)| WeightedTag {
            name,
            weight: w / total,
        })
        .collect();
    tags.sort_by(|a, b| {
        b.weight
            .partial_cmp(&a.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(tags)
}

/// Step 2: Compute obscurity from median listener counts.
async fn build_obscurity_score(
    provider: &dyn ScrobbleProvider,
    top_artists: &[shared::recommendation::RankedArtist],
) -> Result<f64> {
    let artists_to_query: Vec<_> = top_artists.iter().take(30).collect();
    let mut listener_counts: Vec<u64> = Vec::new();

    for artist in &artists_to_query {
        match provider.get_artist_popularity(&artist.name).await {
            Ok(pop) => {
                // Use play_count (total listens) to match the unit from
                // get_global_popularity_median, which also returns total listens.
                if pop.play_count > 0 {
                    listener_counts.push(pop.play_count);
                }
            }
            Err(e) => {
                warn!("Failed to get popularity for '{}': {}", artist.name, e);
            }
        }
    }

    if listener_counts.is_empty() {
        return Ok(0.5); // neutral default
    }

    listener_counts.sort_unstable();
    let user_median = listener_counts[listener_counts.len() / 2];

    let global_median = provider.get_global_popularity_median().await.unwrap_or(1);
    let global_median = global_median.max(1);

    let ratio = user_median as f64 / global_median as f64;
    let score = 1.0 - ratio.clamp(0.0, 1.0);
    Ok(score)
}

/// Step 4: Freshness half-life from recent listen patterns.
async fn build_freshness_half_life(provider: &dyn ScrobbleProvider) -> Result<f64> {
    let listens = provider.get_listens(1000).await?;
    if listens.is_empty() {
        return Ok(90.0);
    }

    // Group by artist, compute span in days between first and last listen.
    let mut artist_spans: HashMap<String, (i64, i64)> = HashMap::new();
    for listen in &listens {
        let key = listen.artist.to_lowercase();
        let entry = artist_spans
            .entry(key)
            .or_insert((listen.timestamp, listen.timestamp));
        entry.0 = entry.0.min(listen.timestamp);
        entry.1 = entry.1.max(listen.timestamp);
    }

    let mut span_days: Vec<f64> = artist_spans
        .values()
        .map(|(first, last)| (last - first) as f64 / 86400.0)
        .filter(|d| *d > 0.0) // only artists heard more than once
        .collect();

    if span_days.is_empty() {
        return Ok(90.0);
    }

    span_days.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = span_days[span_days.len() / 2];

    // Clamp to a reasonable range
    Ok(median.clamp(7.0, 365.0))
}

/// Step 5: Find artists gaining momentum (climbing in recent rank vs all-time).
async fn build_momentum_artists(
    provider: &dyn ScrobbleProvider,
    top_artists_alltime: &[shared::recommendation::RankedArtist],
) -> Result<Vec<MomentumArtist>> {
    let mut recent_artists = provider
        .get_top_artists(TimePeriod::Month, 50)
        .await
        .unwrap_or_default();

    // If monthly stats are empty, approximate from recent listens
    if recent_artists.is_empty() {
        let listens = provider.get_listens(200).await.unwrap_or_default();
        let now = chrono::Utc::now().timestamp();
        let thirty_days_ago = now - 30 * 86400;
        let mut counts: HashMap<String, u64> = HashMap::new();
        for l in &listens {
            if l.timestamp >= thirty_days_ago {
                *counts.entry(l.artist.clone()).or_default() += 1;
            }
        }
        let mut sorted: Vec<_> = counts.into_iter().collect();
        sorted.sort_by_key(|a| std::cmp::Reverse(a.1));
        recent_artists = sorted
            .into_iter()
            .take(50)
            .map(|(name, play_count)| shared::recommendation::RankedArtist {
                name,
                mbid: None,
                play_count,
            })
            .collect();
    }

    // Build alltime rank lookup (0-based index = rank)
    let alltime_rank: HashMap<String, usize> = top_artists_alltime
        .iter()
        .enumerate()
        .map(|(i, a)| (a.name.to_lowercase(), i))
        .collect();

    let mut momentum_list: Vec<MomentumArtist> = Vec::new();

    for (recent_rank, artist) in recent_artists.iter().enumerate() {
        let key = artist.name.to_lowercase();
        let momentum = match alltime_rank.get(&key) {
            Some(&at_rank) => {
                if at_rank == 0 {
                    0.0
                } else {
                    (at_rank as f64 - recent_rank as f64) / at_rank as f64
                }
            }
            // Not in alltime at all -- brand new to the user
            None => 1.0,
        };

        if momentum > 0.0 {
            momentum_list.push(MomentumArtist {
                name: artist.name.clone(),
                momentum_score: momentum,
                mbid: None,
            });
        }
    }

    momentum_list.sort_by(|a, b| {
        b.momentum_score
            .partial_cmp(&a.momentum_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    momentum_list.truncate(10);
    Ok(momentum_list)
}

/// Step 7: Split genre distribution into comfort and exploration zones.
async fn build_tag_zones(profile: &mut UserMusicProfile, provider: &dyn ScrobbleProvider) {
    if profile.genre_distribution.is_empty() {
        return;
    }

    // Comfort zone: tags covering the top 60% of cumulative weight
    let mut cumulative = 0.0;
    let mut comfort = Vec::new();
    let mut exploration = Vec::new();

    for tag in &profile.genre_distribution {
        if cumulative < 0.60 {
            comfort.push(tag.name.to_lowercase());
        } else if tag.weight > 0.001 {
            // Bottom 40% of meaningful tags
            exploration.push(tag.name.to_lowercase());
        }
        cumulative += tag.weight;
    }

    // Expand exploration zone with related tags from top 5 comfort tags.
    let explore_seeds: Vec<String> = comfort.iter().take(5).cloned().collect();
    for seed_tag in &explore_seeds {
        match provider.get_related_tags(seed_tag).await {
            Ok(related) => {
                for r in related {
                    let lower = r.to_lowercase();
                    if !comfort.contains(&lower) && !exploration.contains(&lower) {
                        exploration.push(lower);
                    }
                }
            }
            Err(e) => {
                warn!("Failed to get related tags for '{}': {}", seed_tag, e);
            }
        }
    }

    comfort.truncate(15);
    exploration.truncate(10);
    profile.tag_comfort_zone = comfort;
    profile.tag_exploration_zone = exploration;
}
