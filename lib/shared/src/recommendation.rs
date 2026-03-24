use serde::{Deserialize, Serialize};

/// A single listen/scrobble event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Listen {
    pub artist: String,
    pub track: String,
    pub album: Option<String>,
    pub timestamp: i64,
}

/// An artist ranked by play count
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankedArtist {
    pub name: String,
    pub mbid: Option<String>,
    pub play_count: u64,
}

/// A track ranked by play count
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankedTrack {
    pub artist: String,
    pub track: String,
    pub mbid: Option<String>,
    pub play_count: u64,
}

/// A tag/genre with a normalized weight
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeightedTag {
    pub name: String,
    pub weight: f64,
}

/// An artist similar to another
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimilarArtist {
    pub name: String,
    pub mbid: Option<String>,
    pub score: f64,
}

/// A track similar to another
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimilarTrack {
    pub artist: String,
    pub track: String,
    pub mbid: Option<String>,
    pub score: f64,
}

/// Artist popularity metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistPopularity {
    pub listener_count: u64,
    pub play_count: u64,
}

/// Time period for statistics
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum TimePeriod {
    Week,
    Month,
    Quarter,
    HalfYear,
    Year,
    AllTime,
}

/// Discovery profile preset
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
pub enum DiscoveryProfile {
    Conservative,
    #[default]
    Balanced,
    Adventurous,
}

impl std::fmt::Display for DiscoveryProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiscoveryProfile::Conservative => write!(f, "Conservative"),
            DiscoveryProfile::Balanced => write!(f, "Balanced"),
            DiscoveryProfile::Adventurous => write!(f, "Adventurous"),
        }
    }
}

impl std::str::FromStr for DiscoveryProfile {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Conservative" => Ok(DiscoveryProfile::Conservative),
            "Balanced" => Ok(DiscoveryProfile::Balanced),
            "Adventurous" => Ok(DiscoveryProfile::Adventurous),
            _ => Err(format!("Unknown discovery profile: {}", s)),
        }
    }
}

/// Configuration parameters derived from profile preset.
/// These are the tuning knobs described in the plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileConfig {
    pub exploration_budget: f64,
    pub max_per_artist: u32,
    pub new_release_boost: f64,
    pub popularity_penalty_strength: f64,
    pub cross_source_bonus: f64,
    pub hop2_weight: f64,
    pub momentum_weight: f64,
}

impl ProfileConfig {
    pub fn from_profile(profile: DiscoveryProfile) -> Self {
        match profile {
            DiscoveryProfile::Conservative => Self {
                exploration_budget: 0.10,
                max_per_artist: 3,
                new_release_boost: 1.05,
                popularity_penalty_strength: 0.3,
                cross_source_bonus: 1.5,
                hop2_weight: 0.3,
                momentum_weight: 0.5,
            },
            DiscoveryProfile::Balanced => Self {
                exploration_budget: 0.20,
                max_per_artist: 2,
                new_release_boost: 1.15,
                popularity_penalty_strength: 0.6,
                cross_source_bonus: 1.3,
                hop2_weight: 0.5,
                momentum_weight: 0.7,
            },
            DiscoveryProfile::Adventurous => Self {
                exploration_budget: 0.35,
                max_per_artist: 1,
                new_release_boost: 1.25,
                popularity_penalty_strength: 1.0,
                cross_source_bonus: 1.1,
                hop2_weight: 0.8,
                momentum_weight: 1.0,
            },
        }
    }
}

/// User's music profile built from scrobble history.
/// Cached in the database, rebuilt periodically.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserMusicProfile {
    pub genre_distribution: Vec<WeightedTag>,
    pub era_preference: Vec<(String, f64)>, // decade label -> weight
    pub obscurity_score: f64,               // 0.0 = mainstream, 1.0 = deep cuts
    pub repeat_ratio: f64,                  // 0.0 = all repeats, 1.0 = all unique tracks
    pub freshness_half_life_days: f64,      // how fast user moves on from artists
    pub momentum_artists: Vec<MomentumArtist>,
    pub tag_comfort_zone: Vec<String>,
    pub tag_exploration_zone: Vec<String>,
    pub top_artists_hash: String, // hash to detect profile staleness
    #[serde(default)]
    pub known_artist_names: Vec<String>, // lowercased top artist names for filtering
    #[serde(default)]
    pub known_track_keys: Vec<String>, // "artist:track" lowercased keys for filtering
}

/// An artist with momentum (gaining in recent listening vs all-time)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MomentumArtist {
    pub name: String,
    pub momentum_score: f64, // how much they've climbed in recent rank
}

/// A recommendation candidate with score and provenance
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub artist: String,
    pub track: String,
    pub album: Option<String>,
    pub mbid: Option<String>,
    pub score: f64,
    pub signals: Vec<String>,
    pub source: String,
    pub artist_listener_count: Option<u64>,
    pub primary_genre: Option<String>,
    pub release_year: Option<u16>,
}

/// A set of candidates from a single pipeline
#[derive(Debug, Clone, Default)]
pub struct CandidateSet {
    pub candidates: std::collections::HashMap<String, Candidate>,
}

impl CandidateSet {
    pub fn new() -> Self {
        Self {
            candidates: std::collections::HashMap::new(),
        }
    }

    /// Generate a key for deduplication
    pub fn key(artist: &str, track: &str) -> String {
        format!("{}:{}", artist.to_lowercase(), track.to_lowercase())
    }

    /// Insert or update a candidate, merging signals and accumulating score
    pub fn insert(&mut self, candidate: Candidate) {
        let key = Self::key(&candidate.artist, &candidate.track);
        if let Some(existing) = self.candidates.get_mut(&key) {
            existing.score += candidate.score;
            for signal in candidate.signals {
                if !existing.signals.contains(&signal) {
                    existing.signals.push(signal);
                }
            }
            // Preserve first non-None metadata values
            if existing.artist_listener_count.is_none() {
                existing.artist_listener_count = candidate.artist_listener_count;
            }
            if existing.primary_genre.is_none() {
                existing.primary_genre = candidate.primary_genre;
            }
            if existing.release_year.is_none() {
                existing.release_year = candidate.release_year;
            }
        } else {
            self.candidates.insert(key, candidate);
        }
    }

    pub fn len(&self) -> usize {
        self.candidates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    pub fn into_vec(self) -> Vec<Candidate> {
        self.candidates.into_values().collect()
    }

    pub fn max_score(&self) -> f64 {
        self.candidates
            .values()
            .map(|c| c.score)
            .fold(0.0f64, f64::max)
    }
}

/// Structured report of an engine run, capturing every stage's output.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EngineReport {
    pub profile_source: String,
    pub profile_summary: ProfileSummary,
    pub pipeline_reports: Vec<PipelineReport>,
    pub blend_summary: BlendSummary,
    pub freshness_summary: FreshnessSummary,
    pub diversifier_summary: DiversifierSummary,
    pub final_count: usize,
    pub duration_secs: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProfileSummary {
    pub genre_count: usize,
    pub top_genres: Vec<String>,
    pub obscurity_score: f64,
    pub repeat_ratio: f64, // actually uniqueness ratio: 0.0 = all repeats, 1.0 = all unique
    pub freshness_half_life_days: f64,
    pub momentum_artists: Vec<String>,
    pub comfort_tags: usize,
    pub exploration_tags: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PipelineReport {
    pub name: String,
    pub signals: Vec<SignalReport>,
    pub total_candidates: usize,
    #[serde(default)]
    pub mbid_failures: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SignalReport {
    pub name: String,
    pub candidates_produced: usize,
    pub top_candidates: Vec<CandidateSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CandidateSnapshot {
    pub artist: String,
    pub track: String,
    pub score: f64,
    pub genre: Option<String>,
    pub listeners: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BlendSummary {
    pub sources: usize,
    pub total_after_blend: usize,
    pub cross_source_matches: usize,
    pub top_blended: Vec<CandidateSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FreshnessSummary {
    pub known_artists_penalized: usize,
    pub total_candidates: usize,
    pub penalty_factor: f64,
    pub new_release_boosted: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiversifierSummary {
    pub popularity_penalized: usize,
    pub artist_cap_skipped: usize,
    pub genre_quota_skipped: usize,
    pub exploration_backfilled: usize,
    pub effective_exploration_budget: f64,
    pub unique_artists: usize,
    pub unique_genres: usize,
    pub top_selected: Vec<CandidateSnapshot>,
}

impl CandidateSnapshot {
    pub fn from_candidate(c: &Candidate) -> Self {
        Self {
            artist: c.artist.clone(),
            track: c.track.clone(),
            score: (c.score * 1000.0).round() / 1000.0,
            genre: c.primary_genre.clone(),
            listeners: c.artist_listener_count,
        }
    }
}

impl EngineReport {
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "=== Engine Report ({:.1}s) ===\n",
            self.duration_secs
        ));
        out.push_str(&format!("Profile from: {}\n", self.profile_source));

        let p = &self.profile_summary;
        out.push_str(&format!(
            "  Genres: {} (top: {})\n",
            p.genre_count,
            p.top_genres.join(", ")
        ));
        out.push_str(&format!(
            "  Obscurity: {:.2}, Repeat ratio: {:.2}, Half-life: {:.0}d\n",
            p.obscurity_score, p.repeat_ratio, p.freshness_half_life_days
        ));
        if !p.momentum_artists.is_empty() {
            out.push_str(&format!("  Momentum: {}\n", p.momentum_artists.join(", ")));
        }
        out.push_str(&format!(
            "  Tags: {} comfort, {} exploration\n",
            p.comfort_tags, p.exploration_tags
        ));

        for pr in &self.pipeline_reports {
            out.push_str(&format!(
                "\n--- {} ({} candidates) ---\n",
                pr.name, pr.total_candidates
            ));
            if pr.mbid_failures > 0 {
                out.push_str(&format!(
                    "  MBID resolution failures: {}\n",
                    pr.mbid_failures
                ));
            }
            for sig in &pr.signals {
                out.push_str(&format!(
                    "  {} -> {} candidates",
                    sig.name, sig.candidates_produced
                ));
                if let Some(top) = sig.top_candidates.first() {
                    out.push_str(&format!(
                        "  (top: {} - {} [{:.3}])",
                        top.artist, top.track, top.score
                    ));
                }
                out.push('\n');
            }
        }

        let b = &self.blend_summary;
        out.push_str(&format!(
            "\n--- Blend ({} sources -> {} candidates, {} cross-source) ---\n",
            b.sources, b.total_after_blend, b.cross_source_matches
        ));
        for c in &b.top_blended {
            out.push_str(&format!(
                "  {:.3} {} - {}{}\n",
                c.score,
                c.artist,
                c.track,
                c.genre
                    .as_ref()
                    .map(|g| format!(" [{}]", g))
                    .unwrap_or_default()
            ));
        }

        let f = &self.freshness_summary;
        out.push_str(&format!(
            "\n--- Freshness (penalty={:.2}, {} penalized, {} boosted) ---\n",
            f.penalty_factor, f.known_artists_penalized, f.new_release_boosted
        ));

        let d = &self.diversifier_summary;
        out.push_str(&format!(
            "\n--- Diversifier (exploration={:.0}%) ---\n",
            d.effective_exploration_budget * 100.0
        ));
        out.push_str(&format!(
            "  Popularity penalized: {}, Artist-capped: {}, Genre-skipped: {}, Backfilled: {}\n",
            d.popularity_penalized,
            d.artist_cap_skipped,
            d.genre_quota_skipped,
            d.exploration_backfilled
        ));
        out.push_str(&format!(
            "  Result: {} artists, {} genres\n",
            d.unique_artists, d.unique_genres
        ));

        out.push_str(&format!("\n=== Final: {} tracks ===\n", self.final_count));
        for (i, c) in d.top_selected.iter().enumerate() {
            out.push_str(&format!(
                "  {}. {:.3} {} - {}{}{}\n",
                i + 1,
                c.score,
                c.artist,
                c.track,
                c.genre
                    .as_ref()
                    .map(|g| format!(" [{}]", g))
                    .unwrap_or_default(),
                c.listeners
                    .map(|l| format!(" ({}L)", l))
                    .unwrap_or_default()
            ));
        }

        out
    }
}
