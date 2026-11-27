use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt::Display;
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

/// A single listen/scrobble event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Listen {
    pub artist: String,
    pub track: String,
    pub album: Option<String>,
    pub timestamp: i64,
}

/// A tag/genre with a normalized weight
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeightedTag {
    pub name: String,
    pub weight: f64,
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

impl DiscoveryProfile {
    /// Every profile, in the order Settings shows them.
    pub const ALL: [DiscoveryProfile; 3] = [
        DiscoveryProfile::Conservative,
        DiscoveryProfile::Balanced,
        DiscoveryProfile::Adventurous,
    ];

    /// The profiles' names, in that order, as the settings maps and lists write them.
    pub const NAMES: [&'static str; 3] = [
        Self::ALL[0].name(),
        Self::ALL[1].name(),
        Self::ALL[2].name(),
    ];

    pub const fn name(self) -> &'static str {
        match self {
            DiscoveryProfile::Conservative => "Conservative",
            DiscoveryProfile::Balanced => "Balanced",
            DiscoveryProfile::Adventurous => "Adventurous",
        }
    }
}

impl std::fmt::Display for DiscoveryProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl std::str::FromStr for DiscoveryProfile {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        (Self::ALL.into_iter())
            .find(|p| p.name() == s)
            .ok_or_else(|| format!("Unknown discovery profile: {}", s))
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

/// User's music profile built from the listening histories, rebuilt every run.
#[derive(Debug, Clone, Default)]
pub struct UserMusicProfile {
    pub genre_distribution: Vec<WeightedTag>,
    pub obscurity_score: f64, // 0.0 = mainstream, 1.0 = deep cuts
    pub repeat_ratio: f64,    // 0.0 = all repeats, 1.0 = all unique tracks
    pub momentum_artists: Vec<MomentumArtist>,
    pub tag_comfort_zone: Vec<String>,
    pub tag_exploration_zone: Vec<String>,
    pub seed_artists: Vec<PlayedArtist>,
    pub favourite_artists: Vec<ArtistRef>,
    pub known: KnownMusic,
}

impl UserMusicProfile {
    /// Generators start from seed and favourite artists; without either there is nothing to follow.
    pub fn has_seeds(&self) -> bool {
        !self.seed_artists.is_empty() || !self.favourite_artists.is_empty()
    }
}

/// An artist with momentum (gaining in recent listening vs all-time)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MomentumArtist {
    pub name: String,
    pub momentum_score: f64, // how much they've climbed in recent rank
    #[serde(default)]
    pub mbid: Option<String>,
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
    #[serde(default)]
    pub artist_mbid: Option<String>,
    /// Structured provenance rendered/localized by clients (no hardcoded prose on the wire).
    #[serde(default)]
    pub reason: Option<CandidateReason>,
    /// Which cultural/contextual bucket produced this candidate.
    #[serde(default)]
    pub culture_bucket: Option<CultureBucket>,
}

/// A set of candidates from a single pipeline
#[derive(Debug, Clone, Default)]
pub struct CandidateSet {
    pub candidates: std::collections::HashMap<String, Candidate>,
}

/// Normalize a string for cross-source matching.
/// Lowercases, strips non-alphanumeric characters (keeps whitespace),
/// then collapses whitespace. Handles cases like "don't" vs "dont",
/// "AC/DC" vs "ACDC", and whitespace differences between ListenBrainz
/// and MusicBrainz names.
pub fn normalize_for_matching(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `s` without its combining marks, so "SKÁLD" reads as "SKALD" and "Shirō" as "Shiro".
pub fn without_marks(s: &str) -> String {
    s.nfd().filter(|c| !is_combining_mark(*c)).collect()
}

/// `s` as Discovery matches a name or a title: normalize_for_matching without combining
/// marks, since one source writes "SKÁLD" where another writes "SKALD".
pub fn discovery_name(s: &str) -> String {
    normalize_for_matching(&without_marks(s))
}

/// An artist as a source names it, with its MusicBrainz id when known.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtistRef {
    pub name: String,
    pub mbid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TrackRef {
    pub artist: ArtistRef,
    pub title: String,
    pub mbid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayedArtist {
    pub artist: ArtistRef,
    pub plays: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayedTrack {
    pub track: TrackRef,
    pub plays: u64,
}

/// A similar artist; score is in (0, 1], 1 for the best.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Neighbour {
    pub artist: ArtistRef,
    pub score: f64,
}

/// What the user already has, which no generator may recommend.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KnownMusic {
    pub artist_names: BTreeSet<String>,
    pub artist_mbids: BTreeSet<String>,
    pub track_keys: BTreeSet<String>,
    pub recording_mbids: BTreeSet<String>,
}

impl KnownMusic {
    pub fn add_artist(&mut self, a: &ArtistRef) {
        self.artist_names.insert(discovery_name(&a.name));
        if let Some(m) = &a.mbid {
            self.artist_mbids.insert(m.to_lowercase());
        }
    }

    pub fn add_track(&mut self, t: &TrackRef) {
        self.add_artist(&t.artist);
        self.track_keys
            .insert(CandidateSet::key(&t.artist.name, &t.title));
        if let Some(m) = &t.mbid {
            self.recording_mbids.insert(m.to_lowercase());
        }
    }

    pub fn knows_artist(&self, a: &ArtistRef) -> bool {
        self.artist_names.contains(&discovery_name(&a.name))
            || a.mbid
                .as_ref()
                .is_some_and(|m| self.artist_mbids.contains(&m.to_lowercase()))
    }

    pub fn knows_track(&self, t: &TrackRef) -> bool {
        self.track_keys
            .contains(&CandidateSet::key(&t.artist.name, &t.title))
            || t.mbid
                .as_ref()
                .is_some_and(|m| self.recording_mbids.contains(&m.to_lowercase()))
    }

    pub fn extend(&mut self, other: &KnownMusic) {
        self.artist_names.extend(other.artist_names.iter().cloned());
        self.artist_mbids.extend(other.artist_mbids.iter().cloned());
        self.track_keys.extend(other.track_keys.iter().cloned());
        self.recording_mbids
            .extend(other.recording_mbids.iter().cloned());
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordingFacts {
    pub title: String,
    pub artist: ArtistRef,
    pub first_release_year: Option<u16>,
}

/// A source that failed during a run: its first message and how many times it failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceError {
    pub source: String,
    pub message: String,
    pub failures: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SourceErrors(Vec<SourceError>);

impl SourceErrors {
    pub fn record(&mut self, source: &str, message: impl Display) {
        match self.0.iter_mut().find(|e| e.source == source) {
            Some(e) => e.failures += 1,
            None => self.0.push(SourceError {
                source: source.to_string(),
                message: message.to_string(),
                failures: 1,
            }),
        }
    }

    pub fn extend(&mut self, other: SourceErrors) {
        for e in other.0 {
            match self.0.iter_mut().find(|x| x.source == e.source) {
                Some(x) => x.failures += e.failures,
                None => self.0.push(e),
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn into_vec(self) -> Vec<SourceError> {
        self.0
    }
}

/// Grouped by source as `extend` groups them, for a report's errors.
impl From<Vec<SourceError>> for SourceErrors {
    fn from(errors: Vec<SourceError>) -> Self {
        let mut grouped = SourceErrors::default();
        grouped.extend(SourceErrors(errors));
        grouped
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineState {
    #[default]
    Recommended,
    /// No history had plays or stars, and every history answered.
    NoListeningData,
}

pub const NO_LISTENING_DATA: &str = "No listening data yet. Play or star music in Navidrome.";

/// Lowercase and unify quotes, dashes and spacing but keep other punctuation,
/// so "Fishmans+" stays a different name from "Fishmans".
pub fn fold_name(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            '\u{2010}' | '\u{2011}' | '\u{2013}' | '\u{2014}' => '-',
            c => c,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

impl CandidateSet {
    pub fn new() -> Self {
        Self {
            candidates: std::collections::HashMap::new(),
        }
    }

    /// Generate a key for deduplication
    pub fn key(artist: &str, track: &str) -> String {
        format!(
            "{}:{}",
            discovery_name(artist),
            discovery_name(track)
        )
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
            if existing.artist_mbid.is_none() {
                existing.artist_mbid = candidate.artist_mbid;
            }
            if existing.reason.is_none() {
                existing.reason = candidate.reason;
            }
            if existing.culture_bucket.is_none() {
                existing.culture_bucket = candidate.culture_bucket;
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
    pub profile_summary: ProfileSummary,
    pub pipeline_reports: Vec<PipelineReport>,
    pub blend_summary: BlendSummary,
    pub freshness_summary: FreshnessSummary,
    pub diversifier_summary: DiversifierSummary,
    pub final_count: usize,
    pub duration_secs: f64,
    #[serde(default)]
    pub state: EngineState,
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub source_errors: Vec<SourceError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProfileSummary {
    pub genre_count: usize,
    pub top_genres: Vec<String>,
    pub obscurity_score: f64,
    pub repeat_ratio: f64, // actually uniqueness ratio: 0.0 = all repeats, 1.0 = all unique
    pub momentum_artists: Vec<String>,
    pub comfort_tags: usize,
    pub exploration_tags: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PipelineReport {
    pub name: String,
    pub signals: Vec<SignalReport>,
    pub total_candidates: usize,
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
    pub total_candidates: usize,
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
        if !self.sources.is_empty() {
            out.push_str(&format!("Sources: {}\n", self.sources.join(", ")));
        }
        for e in &self.source_errors {
            if e.failures == 1 {
                out.push_str(&format!("{} failed once: {}\n", e.source, e.message));
            } else {
                out.push_str(&format!(
                    "{} failed {} times: {}\n",
                    e.source, e.failures, e.message
                ));
            }
        }
        if self.state == EngineState::NoListeningData {
            out.push_str(&format!("{NO_LISTENING_DATA}\n"));
        }

        let p = &self.profile_summary;
        out.push_str(&format!(
            "  Genres: {} (top: {})\n",
            p.genre_count,
            p.top_genres.join(", ")
        ));
        out.push_str(&format!(
            "  Obscurity: {:.2}, Repeat ratio: {:.2}\n",
            p.obscurity_score, p.repeat_ratio
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
            "\n--- Freshness ({} new releases boosted) ---\n",
            f.new_release_boosted
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

// --- Discovery 2.0: Contextual, Cultural & Seasonal Types ---

/// Which cultural/contextual bucket produced a recommendation candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CultureBucket {
    Local,
    International,
    TasteWildcard,
    OwnedLibrary,
}

impl CultureBucket {
    pub const fn as_str(self) -> &'static str {
        match self {
            CultureBucket::Local => "local",
            CultureBucket::International => "international",
            CultureBucket::TasteWildcard => "taste_wildcard",
            CultureBucket::OwnedLibrary => "owned_library",
        }
    }
}

/// Coarse live weather condition used to activate weather-reactive contextual mixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WeatherCondition {
    Rainy,
    Snowy,
    Stormy,
    Sunny,
}

impl WeatherCondition {
    pub const fn badge_label(self) -> &'static str {
        match self {
            WeatherCondition::Rainy => "RAINY",
            WeatherCondition::Snowy => "SNOW",
            WeatherCondition::Stormy => "STORM",
            WeatherCondition::Sunny => "SUNNY",
        }
    }
}

/// Live weather snapshot attached to weather-triggered contextual mix summaries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeatherSnapshot {
    pub condition: WeatherCondition,
    pub temperature_c: f32,
    pub weather_code: u16,
    pub is_day: bool,
    #[serde(default)]
    pub location_label: Option<String>,
}

/// Maps an Open-Meteo WMO weather interpretation code + temperature + daytime flag into
/// a Soulbeet [`WeatherCondition`]. Returns `None` for neutral weather (overcast, mild clear,
/// clear night, etc.) so weather mixes only appear when the weather genuinely matches.
pub fn classify_wmo_weather(
    weather_code: u16,
    temperature_c: f32,
    is_day: bool,
) -> Option<WeatherCondition> {
    match weather_code {
        95 | 96 | 99 => Some(WeatherCondition::Stormy),
        71 | 73 | 75 | 77 | 85 | 86 => Some(WeatherCondition::Snowy),
        51 | 53 | 55 | 56 | 57 | 61 | 63 | 65 | 66 | 67 | 80 | 81 | 82 => {
            Some(WeatherCondition::Rainy)
        }
        0 | 1 if is_day && temperature_c >= 20.0 => Some(WeatherCondition::Sunny),
        _ => None,
    }
}

/// User cultural & linguistic listening profile combining zero-config inference with explicit
/// preferences.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserCultureProfile {
    #[serde(default)]
    pub home_country: Option<String>,
    #[serde(default)]
    pub preferred_languages: Vec<String>,
    /// Target share of local-culture tracks in seasonal/contextual packs (0.0..=1.0).
    #[serde(default = "default_local_culture_ratio")]
    pub local_culture_ratio: f32,
    /// Auto-inferred ISO-3166 country weights from Navidrome history + MusicBrainz artist areas.
    #[serde(default)]
    pub inferred_area_weights: Vec<(String, f32)>,
    /// Auto-inferred local sub-genres (e.g. "french electro", "chanson").
    #[serde(default)]
    pub inferred_local_tags: Vec<String>,
    /// Optional explicit city label for weather-reactive mixes (e.g. "Lyon, FR").
    /// When `None`, the server falls back to automatic zero-permission IP geolocation.
    #[serde(default)]
    pub weather_city: Option<String>,
    /// Optional resolved latitude for `weather_city`.
    #[serde(default)]
    pub weather_lat: Option<f64>,
    /// Optional resolved longitude for `weather_city`.
    #[serde(default)]
    pub weather_lon: Option<f64>,
}

fn default_local_culture_ratio() -> f32 {
    0.45
}

impl Default for UserCultureProfile {
    fn default() -> Self {
        Self {
            home_country: None,
            preferred_languages: Vec::new(),
            local_culture_ratio: default_local_culture_ratio(),
            inferred_area_weights: Vec::new(),
            inferred_local_tags: Vec::new(),
            weather_city: None,
            weather_lat: None,
            weather_lon: None,
        }
    }
}

impl UserCultureProfile {
    /// Effective ISO country code: explicit `home_country` first, else highest inferred area.
    pub fn effective_country(&self) -> Option<&str> {
        self.home_country
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| self.inferred_area_weights.first().map(|(c, _)| c.as_str()))
    }
}

/// Live ratio steering for seasonal/contextual packs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CultureSteering {
    MoreLocal,
    #[default]
    Balanced,
    MoreInternational,
}

impl CultureSteering {
    pub const fn as_str(self) -> &'static str {
        match self {
            CultureSteering::MoreLocal => "more_local",
            CultureSteering::Balanced => "balanced",
            CultureSteering::MoreInternational => "more_international",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "more_local" | "local" => CultureSteering::MoreLocal,
            "more_international" | "international" | "intl" => CultureSteering::MoreInternational,
            _ => CultureSteering::Balanced,
        }
    }

    /// Computes the effective local-culture track share given a base ratio (0.0..=1.0).
    pub fn effective_ratio(self, base: f32) -> f32 {
        let base = base.clamp(0.0, 1.0);
        match self {
            CultureSteering::MoreLocal => (base + 0.25).clamp(0.15, 0.85),
            CultureSteering::Balanced => base,
            CultureSteering::MoreInternational => (base - 0.25).clamp(0.10, 0.85),
        }
    }
}

/// Structured, client-localizable explanation for why a track was recommended or included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CandidateReason {
    /// Regional cultural pick for a contextual mix (client formats with mix + country code).
    RegionalContextual {
        mix_slug: String,
        #[serde(default)]
        country_code: Option<String>,
    },
    /// International classic for a contextual mix.
    InternationalContextual { mix_slug: String },
    /// Crossover between a contextual mix and one of the user's top genres.
    TasteWildcard { mix_slug: String, genre: String },
    /// Track matched a moment/mood mix's tags and/or user genre.
    MomentMatch {
        mix_slug: String,
        #[serde(default)]
        mood_tag: Option<String>,
        #[serde(default)]
        genre: Option<String>,
    },
    /// Owned library track matching a contextual mix's mood/genre criteria.
    LibraryMatch {
        mix_slug: String,
        #[serde(default)]
        matched_genre: Option<String>,
    },
    /// Seed track that started a radio session.
    RadioSeed { seed_artist: String },
    /// Track from the same artist as the radio seed.
    MoreFromArtist { artist: String },
    /// Track from an artist similar to the radio seed.
    SimilarArtist { seed_artist: String, artist: String },
}

/// Annual calendar window (inclusive) for a seasonal contextual mix, supporting year-wrap
/// windows such as Dec 28 – Jan 2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarWindow {
    pub start_month: u8,
    pub start_day: u8,
    pub end_month: u8,
    pub end_day: u8,
    /// Days after `end_month/end_day` before unkept seasonal tracks and playlists auto-clean.
    #[serde(default = "default_cleanup_grace_days")]
    pub cleanup_grace_days: u16,
}

fn default_cleanup_grace_days() -> u16 {
    7
}

impl CalendarWindow {
    pub const fn new(
        start_month: u8,
        start_day: u8,
        end_month: u8,
        end_day: u8,
        cleanup_grace_days: u16,
    ) -> Self {
        Self {
            start_month,
            start_day,
            end_month,
            end_day,
            cleanup_grace_days,
        }
    }

    /// Returns `true` when `(month, day)` (1-based) falls inside this window.
    pub fn is_active(&self, month: u32, day: u32) -> bool {
        let cur = (month, day);
        let start = (u32::from(self.start_month), u32::from(self.start_day));
        let end = (u32::from(self.end_month), u32::from(self.end_day));
        if start <= end {
            cur >= start && cur <= end
        } else {
            cur >= start || cur <= end
        }
    }

    /// Returns `true` when months are 1..=12 and days are valid for their month (allowing Feb 29).
    pub fn is_valid(&self) -> bool {
        let sm = u32::from(self.start_month);
        let sd = u32::from(self.start_day);
        let em = u32::from(self.end_month);
        let ed = u32::from(self.end_day);
        (1..=12).contains(&sm)
            && (1..=12).contains(&em)
            && sd >= 1
            && sd <= days_in_month(2024, sm)
            && ed >= 1
            && ed <= days_in_month(2024, em)
    }

    /// Computes concrete ISO-8601 `YYYY-MM-DD` `(active_from, active_until, expires_at)` dates
    /// for the cycle containing `(year, month, day)`.
    pub fn cycle_dates(&self, year: i32, month: u32, day: u32) -> Option<(String, String, String)> {
        if !self.is_valid() {
            return None;
        }
        let start_md = (u32::from(self.start_month), u32::from(self.start_day));
        let end_md = (u32::from(self.end_month), u32::from(self.end_day));
        let wraps = start_md > end_md;

        let (start_year, end_year) = if wraps {
            if (month, day) <= end_md {
                (year - 1, year)
            } else {
                (year, year + 1)
            }
        } else {
            (year, year)
        };

        let sm = u32::from(self.start_month);
        let sd = u32::from(self.start_day).min(days_in_month(start_year, sm));
        let em = u32::from(self.end_month);
        let ed = u32::from(self.end_day).min(days_in_month(end_year, em));
        let (xy, xm, xd) = add_days(end_year, em, ed, u32::from(self.cleanup_grace_days));
        Some((
            format!("{start_year:04}-{sm:02}-{sd:02}"),
            format!("{end_year:04}-{em:02}-{ed:02}"),
            format!("{xy:04}-{xm:02}-{xd:02}"),
        ))
    }

    /// Inclusive length of this calendar window in days (non-leap year reference).
    /// Used to prioritize narrow holiday/event windows (e.g. Valentine's, Halloween, Midsummer)
    /// ahead of broad multi-month seasonal mixes in hero slots.
    pub fn span_days(&self) -> u32 {
        if !self.is_valid() {
            return 365;
        }
        let start = day_of_year(u32::from(self.start_month), u32::from(self.start_day));
        let end = day_of_year(u32::from(self.end_month), u32::from(self.end_day));
        if start <= end {
            end - start + 1
        } else {
            (365 - start + 1) + end
        }
    }
}

fn day_of_year(month: u32, day: u32) -> u32 {
    let mut total = 0u32;
    for m in 1..month {
        total += days_in_month(2026, m);
    }
    total + day.min(days_in_month(2026, month))
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn add_days(mut year: i32, mut month: u32, mut day: u32, mut extra: u32) -> (i32, u32, u32) {
    while extra > 0 {
        let dim = days_in_month(year, month);
        if day + extra <= dim {
            day += extra;
            break;
        }
        extra -= dim - day + 1;
        day = 1;
        if month == 12 {
            month = 1;
            year += 1;
        } else {
            month += 1;
        }
    }
    (year, month, day)
}

/// Time-of-day window (24h, inclusive, supports midnight wrap such as 22..=4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HourWindow {
    pub start_hour: u8,
    pub end_hour: u8,
}

impl HourWindow {
    pub const fn new(start_hour: u8, end_hour: u8) -> Self {
        Self {
            start_hour,
            end_hour,
        }
    }

    pub fn is_valid(&self) -> bool {
        self.start_hour <= 23 && self.end_hour <= 23
    }

    pub fn is_active(&self, hour: u32) -> bool {
        let start = u32::from(self.start_hour);
        let end = u32::from(self.end_hour);
        if start <= end {
            hour >= start && hour <= end
        } else {
            hour >= start || hour <= end
        }
    }
}

/// Ordering mode for library tracks inside a contextual/smart mix before daily tier jitter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MixSortOrder {
    #[default]
    RatingThenPlays,
    OldestPlayedFirst,
    NewestAddedFirst,
}

/// Optional structured selection filters for smart/library-rule contextual mixes
/// (e.g. Forgotten Gems, Fresh Arrivals, Decade Capsules).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MixSelectionRules {
    /// Inclusive release year range (e.g. `(1980, 1989)` for 80s Wave).
    #[serde(default)]
    pub year_range: Option<(u16, u16)>,
    /// Minimum user rating (1..=5) for favorite qualification.
    /// When `min_rating` and/or `min_play_count` are set, a song qualifies as a favorite if it is
    /// starred OR meets either configured threshold (so starred, rated, and heavily-played tracks
    /// all surface cleanly).
    #[serde(default)]
    pub min_rating: Option<u8>,
    /// Minimum play count for favorite qualification (ORed with `min_rating` and starred status).
    #[serde(default)]
    pub min_play_count: Option<u32>,
    /// Maximum play count (e.g. `Some(1)` for Fresh Arrivals / unheard tracks).
    #[serde(default)]
    pub max_play_count: Option<u32>,
    /// Require the song to have been last played at least N days ago (Forgotten Gems).
    #[serde(default)]
    pub not_played_days: Option<u32>,
    /// Require the song to have been added to the library within the last N days (Fresh Arrivals).
    #[serde(default)]
    pub added_within_days: Option<u32>,
    /// When true, never fall back to unrelated library tracks if filters yield zero matches.
    #[serde(default)]
    pub strict_match: bool,
    /// Primary ordering before daily deterministic tier jitter.
    #[serde(default)]
    pub sort_order: MixSortOrder,
}

/// Extensible, data-driven contextual mix definition (seasonal pack or moment/mood mix).
/// Built-in definitions leave `custom_name` / `custom_description` as `None` so every client
/// localizes labels cleanly from `i18n_key` + typed metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextualMixDefinition {
    pub slug: String,
    #[serde(default)]
    pub owner_user_id: Option<String>,
    pub kind: ContextualMixKind,
    /// Client localization key (e.g. `"mix.seasonal.winter_holiday"`, `"mix.moment.drive"`).
    pub i18n_key: String,
    /// Optional explicit user/admin-chosen name (overrides `i18n_key` on clients when set).
    #[serde(default)]
    pub custom_name: Option<String>,
    #[serde(default)]
    pub custom_description: Option<String>,
    #[serde(default)]
    pub built_in: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Multi-tag theme seeds (ListenBrainz / MusicBrainz folksonomy tags).
    #[serde(default)]
    pub theme_tags: Vec<String>,
    /// Global playlist search queries for international community consensus pools.
    #[serde(default)]
    pub international_queries: Vec<String>,
    /// Active calendar dates for seasonal mixes.
    #[serde(default)]
    pub calendar_window: Option<CalendarWindow>,
    /// Active hour range for time-of-day moment mixes.
    #[serde(default)]
    pub hour_window: Option<HourWindow>,
    /// Active ISO weekdays (`1` = Monday .. `7` = Sunday) for day-of-week moment mixes.
    /// Empty means active on every day of the week.
    #[serde(default)]
    pub days_of_week: Vec<u8>,
    /// Live weather conditions that activate this mix (empty = not weather-gated).
    #[serde(default)]
    pub weather_conditions: Vec<WeatherCondition>,
    /// Always surface this mix regardless of time-of-day (e.g. Drive Mix).
    #[serde(default)]
    pub always_surfaced: bool,
    /// Empty = active globally; non-empty = auto-prioritized for matching ISO-3166 country codes.
    #[serde(default)]
    pub countries: Vec<String>,
    /// Target fraction of owned library tracks (0.0 for pure discovery, ~0.75 for moment mixes).
    #[serde(default)]
    pub owned_ratio: f32,
    /// Base local-culture track share when cultural blending is enabled (e.g. `Some(0.45)`).
    #[serde(default)]
    pub default_local_ratio: Option<f32>,
    /// Keep listening/staging isolated from core taste profile (true for seasonal packs).
    #[serde(default)]
    pub exclude_from_core_taste: bool,
    /// Prioritize locally cached offline tracks (e.g. Drive Mix / Android Auto).
    #[serde(default)]
    pub offline_priority: bool,
    #[serde(default = "default_target_tracks")]
    pub target_tracks: u32,
    /// Whether to materialize/sync this mix as a Navidrome playlist.
    #[serde(default = "default_true")]
    pub sync_to_navidrome: bool,
    /// Optional structured library selection rules (decades, forgotten gems, fresh arrivals).
    #[serde(default)]
    pub selection_rules: Option<MixSelectionRules>,
}

fn default_true() -> bool {
    true
}

fn default_target_tracks() -> u32 {
    25
}

impl ContextualMixDefinition {
    pub fn primary_tag(&self) -> Option<&str> {
        self.theme_tags.first().map(String::as_str)
    }

    /// Resolves the base local-culture ratio for this mix given the user's cultural profile.
    /// Returns `None` when cultural blending is disabled on the mix (`default_local_ratio` is
    /// `None`). When enabled (`Some(base)`), respects `culture.local_culture_ratio` unless `base`
    /// is explicitly `0.0`.
    pub fn effective_base_local_ratio(&self, culture: &UserCultureProfile) -> Option<f32> {
        self.default_local_ratio.map(|base| {
            if base > 0.0 {
                culture.local_culture_ratio.clamp(0.0, 1.0)
            } else {
                0.0
            }
        })
    }

    pub fn matches_country(&self, effective_country: Option<&str>) -> bool {
        if self.countries.is_empty() {
            return true;
        }
        let Some(code) = effective_country else {
            return false;
        };
        self.countries
            .iter()
            .any(|c| c.eq_ignore_ascii_case(code))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn is_active_at(
        &self,
        month: u32,
        day: u32,
        hour: u32,
        weekday: Option<u32>,
        weather: Option<WeatherCondition>,
        effective_country: Option<&str>,
        explicitly_enabled_by_user: bool,
    ) -> bool {
        if !self.enabled && !explicitly_enabled_by_user {
            return false;
        }
        match self.kind {
            ContextualMixKind::Seasonal => {
                let in_window = self
                    .calendar_window
                    .as_ref()
                    .is_none_or(|w| w.is_active(month, day));
                in_window && (explicitly_enabled_by_user || self.matches_country(effective_country))
            }
            ContextualMixKind::Moment => {
                if !self.weather_conditions.is_empty() {
                    let weather_ok =
                        weather.is_some_and(|w| self.weather_conditions.contains(&w));
                    if !weather_ok {
                        return false;
                    }
                    let day_ok = self.days_of_week.is_empty()
                        || weekday.is_some_and(|w| {
                            self.days_of_week.iter().any(|&d| u32::from(d) == w)
                        });
                    let hour_ok = self
                        .hour_window
                        .as_ref()
                        .is_none_or(|w| w.is_active(hour));
                    return day_ok && hour_ok;
                }
                if self.always_surfaced {
                    return true;
                }
                let day_ok = self.days_of_week.is_empty()
                    || weekday
                        .is_some_and(|w| self.days_of_week.iter().any(|&d| u32::from(d) == w));
                let hour_ok = self
                    .hour_window
                    .as_ref()
                    .is_none_or(|w| w.is_active(hour));
                day_ok && hour_ok
            }
            ContextualMixKind::Rolling => true,
        }
    }
}

/// Canonical global catalog of built-in contextual mixes seeded into SQLite on first access.
/// Admins and users can add, edit, or disable mixes at runtime without recompiling.
pub fn builtin_contextual_mixes() -> Vec<ContextualMixDefinition> {
    let s = |v: &[&str]| v.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
    let seasonal = |slug: &str,
                    tags: &[&str],
                    queries: &[&str],
                    window: CalendarWindow,
                    local_ratio: f32,
                    target_tracks: u32| ContextualMixDefinition {
        slug: slug.into(),
        owner_user_id: None,
        kind: ContextualMixKind::Seasonal,
        i18n_key: format!("mix.seasonal.{slug}"),
        custom_name: None,
        custom_description: None,
        built_in: true,
        enabled: true,
        theme_tags: s(tags),
        international_queries: s(queries),
        calendar_window: Some(window),
        hour_window: None,
        days_of_week: vec![],
        weather_conditions: vec![],
        always_surfaced: false,
        countries: vec![],
        owned_ratio: 0.0,
        default_local_ratio: Some(local_ratio),
        exclude_from_core_taste: true,
        offline_priority: false,
        target_tracks,
        sync_to_navidrome: true,
        selection_rules: None,
    };

    vec![
        // --- Seasonal Mixes ---
        seasonal(
            "winter_holiday",
            &["christmas", "holiday", "winter"],
            &["Christmas Hits", "Winter Holiday Classics"],
            CalendarWindow::new(11, 20, 12, 27, 9),
            0.45,
            25,
        ),
        seasonal(
            "new_years_eve",
            &["dance", "party", "disco", "funk", "electronic"],
            &["Party Hits", "Dance Floor Classics"],
            CalendarWindow::new(12, 28, 1, 2, 6),
            0.45,
            25,
        ),
        seasonal(
            "cozy_winter",
            &["winter", "folk", "ambient", "indie folk", "acoustic"],
            &["Winter Acoustic", "Cozy Winter"],
            CalendarWindow::new(1, 3, 2, 6, 5),
            0.40,
            25,
        ),
        seasonal(
            "lunar_new_year",
            &["mandopop", "cantopop", "k-pop", "c-pop", "festive"],
            &["Lunar New Year", "Asian Pop Hits"],
            CalendarWindow::new(1, 20, 2, 20, 7),
            0.55,
            20,
        ),
        seasonal(
            "valentines",
            &["soul", "r&b", "jazz", "chanson", "romance"],
            &["Valentine", "Love Songs"],
            CalendarWindow::new(2, 7, 2, 15, 4),
            0.45,
            20,
        ),
        seasonal(
            "carnival",
            &["samba", "salsa", "cumbia", "afrobeats", "carnival"],
            &["Carnival Hits", "Samba Party"],
            CalendarWindow::new(2, 1, 3, 5, 7),
            0.55,
            20,
        ),
        seasonal(
            "celtic_spring",
            &["celtic", "irish", "celtic folk", "irish folk", "celtic punk"],
            &["Irish Folk", "St Patrick"],
            CalendarWindow::new(3, 10, 3, 19, 4),
            0.45,
            20,
        ),
        seasonal(
            "spring",
            &["spring", "indie pop", "dream pop", "soul", "bossa nova"],
            &["Fresh Indie", "Spring Acoustic"],
            CalendarWindow::new(3, 20, 5, 31, 7),
            0.45,
            25,
        ),
        seasonal(
            "summer",
            &["summer", "tropical", "reggae", "indie", "pop"],
            &["Summer Anthems", "Sun-Soaked Indie"],
            CalendarWindow::new(6, 1, 8, 31, 7),
            0.45,
            25,
        ),
        seasonal(
            "summer_solstice",
            &["indie", "electro", "french touch", "folk", "festival"],
            &["Midsummer", "Summer Solstice"],
            CalendarWindow::new(6, 15, 6, 24, 4),
            0.45,
            20,
        ),
        seasonal(
            "late_summer_reset",
            &["indie pop", "dream pop", "shoegaze", "synthpop", "alternative"],
            &["Late Summer", "Indie September"],
            CalendarWindow::new(9, 1, 9, 21, 5),
            0.45,
            25,
        ),
        seasonal(
            "autumn",
            &["autumn", "acoustic", "folk", "jazz", "chill"],
            &["Autumn Chill", "Cozy Acoustic"],
            CalendarWindow::new(9, 22, 11, 19, 7),
            0.45,
            25,
        ),
        seasonal(
            "halloween",
            &["halloween", "darkwave", "post-punk", "synthwave"],
            &["Halloween Classics", "Darkwave Night"],
            CalendarWindow::new(10, 15, 11, 1, 7),
            0.40,
            25,
        ),
        // --- Time-of-Day & Activity Moment Mixes ---
        ContextualMixDefinition {
            slug: "drive".into(),
            owner_user_id: None,
            kind: ContextualMixKind::Moment,
            i18n_key: "mix.moment.drive".into(),
            custom_name: None,
            custom_description: None,
            built_in: true,
            enabled: true,
            theme_tags: s(&["indie rock", "electronic", "synthpop", "funk", "hip hop"]),
            international_queries: vec![],
            calendar_window: None,
            hour_window: None,
            days_of_week: vec![],
            weather_conditions: vec![],
            always_surfaced: true,
            countries: vec![],
            owned_ratio: 0.75,
            default_local_ratio: None,
            exclude_from_core_taste: false,
            offline_priority: true,
            target_tracks: 25,
            sync_to_navidrome: false,
            selection_rules: None,
        },
        ContextualMixDefinition {
            slug: "morning".into(),
            owner_user_id: None,
            kind: ContextualMixKind::Moment,
            i18n_key: "mix.moment.morning".into(),
            custom_name: None,
            custom_description: None,
            built_in: true,
            enabled: true,
            theme_tags: s(&["pop", "indie pop", "funk", "soul", "bossa nova"]),
            international_queries: vec![],
            calendar_window: None,
            hour_window: Some(HourWindow::new(5, 10)),
            days_of_week: vec![],
            weather_conditions: vec![],
            always_surfaced: false,
            countries: vec![],
            owned_ratio: 0.75,
            default_local_ratio: None,
            exclude_from_core_taste: false,
            offline_priority: false,
            target_tracks: 25,
            sync_to_navidrome: false,
            selection_rules: None,
        },
        ContextualMixDefinition {
            slug: "focus".into(),
            owner_user_id: None,
            kind: ContextualMixKind::Moment,
            i18n_key: "mix.moment.focus".into(),
            custom_name: None,
            custom_description: None,
            built_in: true,
            enabled: true,
            theme_tags: s(&["ambient", "downtempo", "instrumental", "jazz", "lo-fi"]),
            international_queries: vec![],
            calendar_window: None,
            hour_window: Some(HourWindow::new(11, 17)),
            days_of_week: vec![],
            weather_conditions: vec![],
            always_surfaced: false,
            countries: vec![],
            owned_ratio: 0.75,
            default_local_ratio: None,
            exclude_from_core_taste: false,
            offline_priority: false,
            target_tracks: 25,
            sync_to_navidrome: false,
            selection_rules: None,
        },
        ContextualMixDefinition {
            slug: "evening_unwind".into(),
            owner_user_id: None,
            kind: ContextualMixKind::Moment,
            i18n_key: "mix.moment.evening_unwind".into(),
            custom_name: None,
            custom_description: None,
            built_in: true,
            enabled: true,
            theme_tags: s(&["soul", "jazz", "r&b", "indie", "folk", "acoustic"]),
            international_queries: vec![],
            calendar_window: None,
            hour_window: Some(HourWindow::new(18, 21)),
            days_of_week: vec![],
            weather_conditions: vec![],
            always_surfaced: false,
            countries: vec![],
            owned_ratio: 0.75,
            default_local_ratio: None,
            exclude_from_core_taste: false,
            offline_priority: false,
            target_tracks: 25,
            sync_to_navidrome: false,
            selection_rules: None,
        },
        ContextualMixDefinition {
            slug: "dinner_groove".into(),
            owner_user_id: None,
            kind: ContextualMixKind::Moment,
            i18n_key: "mix.moment.dinner_groove".into(),
            custom_name: None,
            custom_description: None,
            built_in: true,
            enabled: true,
            theme_tags: s(&["funk", "disco", "soul", "bossa nova", "nu-disco", "jazz"]),
            international_queries: vec![],
            calendar_window: None,
            hour_window: Some(HourWindow::new(18, 22)),
            days_of_week: vec![],
            weather_conditions: vec![],
            always_surfaced: false,
            countries: vec![],
            owned_ratio: 0.80,
            default_local_ratio: None,
            exclude_from_core_taste: false,
            offline_priority: false,
            target_tracks: 25,
            sync_to_navidrome: false,
            selection_rules: None,
        },
        ContextualMixDefinition {
            slug: "late_night".into(),
            owner_user_id: None,
            kind: ContextualMixKind::Moment,
            i18n_key: "mix.moment.late_night".into(),
            custom_name: None,
            custom_description: None,
            built_in: true,
            enabled: true,
            theme_tags: s(&["trip hop", "dream pop", "chillout", "jazz", "r&b"]),
            international_queries: vec![],
            calendar_window: None,
            hour_window: Some(HourWindow::new(22, 4)),
            days_of_week: vec![],
            weather_conditions: vec![],
            always_surfaced: false,
            countries: vec![],
            owned_ratio: 0.75,
            default_local_ratio: None,
            exclude_from_core_taste: false,
            offline_priority: false,
            target_tracks: 25,
            sync_to_navidrome: false,
            selection_rules: None,
        },
        // --- Weather-Reactive Moment Mixes (only shown when live local weather matches) ---
        ContextualMixDefinition {
            slug: "rainy_day".into(),
            owner_user_id: None,
            kind: ContextualMixKind::Moment,
            i18n_key: "mix.moment.rainy_day".into(),
            custom_name: None,
            custom_description: None,
            built_in: true,
            enabled: true,
            theme_tags: s(&[
                "acoustic",
                "folk",
                "singer-songwriter",
                "indie folk",
                "piano",
                "ambient",
            ]),
            international_queries: vec![],
            calendar_window: None,
            hour_window: None,
            days_of_week: vec![],
            weather_conditions: vec![WeatherCondition::Rainy],
            always_surfaced: false,
            countries: vec![],
            owned_ratio: 0.75,
            default_local_ratio: None,
            exclude_from_core_taste: false,
            offline_priority: false,
            target_tracks: 25,
            sync_to_navidrome: false,
            selection_rules: None,
        },
        ContextualMixDefinition {
            slug: "snow_day".into(),
            owner_user_id: None,
            kind: ContextualMixKind::Moment,
            i18n_key: "mix.moment.snow_day".into(),
            custom_name: None,
            custom_description: None,
            built_in: true,
            enabled: true,
            theme_tags: s(&[
                "ambient",
                "folk",
                "nordic",
                "classical",
                "piano",
                "jazz",
                "indie folk",
            ]),
            international_queries: vec![],
            calendar_window: None,
            hour_window: None,
            days_of_week: vec![],
            weather_conditions: vec![WeatherCondition::Snowy],
            always_surfaced: false,
            countries: vec![],
            owned_ratio: 0.75,
            default_local_ratio: None,
            exclude_from_core_taste: false,
            offline_priority: false,
            target_tracks: 25,
            sync_to_navidrome: false,
            selection_rules: None,
        },
        ContextualMixDefinition {
            slug: "storm_night".into(),
            owner_user_id: None,
            kind: ContextualMixKind::Moment,
            i18n_key: "mix.moment.storm_night".into(),
            custom_name: None,
            custom_description: None,
            built_in: true,
            enabled: true,
            theme_tags: s(&[
                "post-rock",
                "trip hop",
                "shoegaze",
                "dark ambient",
                "blues",
                "alternative",
            ]),
            international_queries: vec![],
            calendar_window: None,
            hour_window: None,
            days_of_week: vec![],
            weather_conditions: vec![WeatherCondition::Stormy],
            always_surfaced: false,
            countries: vec![],
            owned_ratio: 0.75,
            default_local_ratio: None,
            exclude_from_core_taste: false,
            offline_priority: false,
            target_tracks: 25,
            sync_to_navidrome: false,
            selection_rules: None,
        },
        ContextualMixDefinition {
            slug: "sunny_day".into(),
            owner_user_id: None,
            kind: ContextualMixKind::Moment,
            i18n_key: "mix.moment.sunny_day".into(),
            custom_name: None,
            custom_description: None,
            built_in: true,
            enabled: true,
            theme_tags: s(&[
                "funk",
                "soul",
                "disco",
                "indie pop",
                "bossa nova",
                "reggae",
                "pop",
            ]),
            international_queries: vec![],
            calendar_window: None,
            hour_window: None,
            days_of_week: vec![],
            weather_conditions: vec![WeatherCondition::Sunny],
            always_surfaced: false,
            countries: vec![],
            owned_ratio: 0.75,
            default_local_ratio: None,
            exclude_from_core_taste: false,
            offline_priority: false,
            target_tracks: 25,
            sync_to_navidrome: false,
            selection_rules: None,
        },
        // --- Day-of-Week Moment Mixes ---
        ContextualMixDefinition {
            slug: "friday_night".into(),
            owner_user_id: None,
            kind: ContextualMixKind::Moment,
            i18n_key: "mix.moment.friday_night".into(),
            custom_name: None,
            custom_description: None,
            built_in: true,
            enabled: true,
            theme_tags: s(&[
                "electronic",
                "disco",
                "funk",
                "house",
                "hip hop",
                "synthpop",
                "indie rock",
            ]),
            international_queries: vec![],
            calendar_window: None,
            hour_window: Some(HourWindow::new(18, 23)),
            days_of_week: vec![5, 6],
            weather_conditions: vec![],
            always_surfaced: false,
            countries: vec![],
            owned_ratio: 0.75,
            default_local_ratio: None,
            exclude_from_core_taste: false,
            offline_priority: false,
            target_tracks: 25,
            sync_to_navidrome: false,
            selection_rules: None,
        },
        ContextualMixDefinition {
            slug: "sunday_morning".into(),
            owner_user_id: None,
            kind: ContextualMixKind::Moment,
            i18n_key: "mix.moment.sunday_morning".into(),
            custom_name: None,
            custom_description: None,
            built_in: true,
            enabled: true,
            theme_tags: s(&[
                "acoustic",
                "jazz",
                "soul",
                "folk",
                "bossa nova",
                "chanson",
                "ambient",
            ]),
            international_queries: vec![],
            calendar_window: None,
            hour_window: Some(HourWindow::new(7, 12)),
            days_of_week: vec![7],
            weather_conditions: vec![],
            always_surfaced: false,
            countries: vec![],
            owned_ratio: 0.85,
            default_local_ratio: None,
            exclude_from_core_taste: false,
            offline_priority: false,
            target_tracks: 25,
            sync_to_navidrome: false,
            selection_rules: None,
        },
        // --- Library-Rule Smart Mixes (hidden cleanly when library has 0 matching tracks) ---
        ContextualMixDefinition {
            slug: "forgotten_gems".into(),
            owner_user_id: None,
            kind: ContextualMixKind::Moment,
            i18n_key: "mix.moment.forgotten_gems".into(),
            custom_name: None,
            custom_description: None,
            built_in: true,
            enabled: true,
            theme_tags: vec![],
            international_queries: vec![],
            calendar_window: None,
            hour_window: None,
            days_of_week: vec![],
            weather_conditions: vec![],
            always_surfaced: true,
            countries: vec![],
            owned_ratio: 1.0,
            default_local_ratio: None,
            exclude_from_core_taste: false,
            offline_priority: false,
            target_tracks: 25,
            sync_to_navidrome: false,
            selection_rules: Some(MixSelectionRules {
                min_rating: Some(4),
                min_play_count: Some(5),
                not_played_days: Some(180),
                strict_match: true,
                sort_order: MixSortOrder::OldestPlayedFirst,
                ..Default::default()
            }),
        },
        ContextualMixDefinition {
            slug: "fresh_arrivals".into(),
            owner_user_id: None,
            kind: ContextualMixKind::Moment,
            i18n_key: "mix.moment.fresh_arrivals".into(),
            custom_name: None,
            custom_description: None,
            built_in: true,
            enabled: true,
            theme_tags: vec![],
            international_queries: vec![],
            calendar_window: None,
            hour_window: None,
            days_of_week: vec![],
            weather_conditions: vec![],
            always_surfaced: true,
            countries: vec![],
            owned_ratio: 1.0,
            default_local_ratio: None,
            exclude_from_core_taste: false,
            offline_priority: false,
            target_tracks: 25,
            sync_to_navidrome: false,
            selection_rules: Some(MixSelectionRules {
                added_within_days: Some(60),
                max_play_count: Some(1),
                strict_match: true,
                sort_order: MixSortOrder::NewestAddedFirst,
                ..Default::default()
            }),
        },
        ContextualMixDefinition {
            slug: "decade_80s".into(),
            owner_user_id: None,
            kind: ContextualMixKind::Moment,
            i18n_key: "mix.moment.decade_80s".into(),
            custom_name: None,
            custom_description: None,
            built_in: true,
            enabled: true,
            theme_tags: vec![],
            international_queries: vec![],
            calendar_window: None,
            hour_window: None,
            days_of_week: vec![],
            weather_conditions: vec![],
            always_surfaced: true,
            countries: vec![],
            owned_ratio: 1.0,
            default_local_ratio: None,
            exclude_from_core_taste: false,
            offline_priority: false,
            target_tracks: 25,
            sync_to_navidrome: false,
            selection_rules: Some(MixSelectionRules {
                year_range: Some((1980, 1989)),
                strict_match: true,
                ..Default::default()
            }),
        },
        ContextualMixDefinition {
            slug: "decade_90s".into(),
            owner_user_id: None,
            kind: ContextualMixKind::Moment,
            i18n_key: "mix.moment.decade_90s".into(),
            custom_name: None,
            custom_description: None,
            built_in: true,
            enabled: true,
            theme_tags: vec![],
            international_queries: vec![],
            calendar_window: None,
            hour_window: None,
            days_of_week: vec![],
            weather_conditions: vec![],
            always_surfaced: true,
            countries: vec![],
            owned_ratio: 1.0,
            default_local_ratio: None,
            exclude_from_core_taste: false,
            offline_priority: false,
            target_tracks: 25,
            sync_to_navidrome: false,
            selection_rules: Some(MixSelectionRules {
                year_range: Some((1990, 1999)),
                strict_match: true,
                ..Default::default()
            }),
        },
        ContextualMixDefinition {
            slug: "decade_2000s".into(),
            owner_user_id: None,
            kind: ContextualMixKind::Moment,
            i18n_key: "mix.moment.decade_2000s".into(),
            custom_name: None,
            custom_description: None,
            built_in: true,
            enabled: true,
            theme_tags: vec![],
            international_queries: vec![],
            calendar_window: None,
            hour_window: None,
            days_of_week: vec![],
            weather_conditions: vec![],
            always_surfaced: true,
            countries: vec![],
            owned_ratio: 1.0,
            default_local_ratio: None,
            exclude_from_core_taste: false,
            offline_priority: false,
            target_tracks: 25,
            sync_to_navidrome: false,
            selection_rules: Some(MixSelectionRules {
                year_range: Some((2000, 2009)),
                strict_match: true,
                ..Default::default()
            }),
        },
    ]
}

/// Canonical, neutral display title for built-in and custom contextual mixes.
///
/// Honors a non-empty `custom_name` first, then matches `i18n_key` (or `slug`) against the
/// canonical catalog without injecting country-specific phrasing or country suffixes into the
/// title. Regional blend ratios belong in the mix metadata/badges rather than the title.
pub fn canonical_mix_title(
    slug: &str,
    i18n_key: Option<&str>,
    custom_name: Option<&str>,
) -> String {
    if let Some(custom) = custom_name.map(str::trim).filter(|s| !s.is_empty()) {
        return custom.to_string();
    }

    let lookup = |raw: &str| -> Option<&'static str> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "mix.seasonal.winter_holiday" | "winter_holiday" | "noel" | "christmas" => {
                Some("Christmas Mix")
            }
            "mix.seasonal.new_years_eve" | "new_years_eve" | "nye" => Some("New Year's Eve"),
            "mix.seasonal.cozy_winter" | "cozy_winter" => Some("Cozy Winter"),
            "mix.seasonal.lunar_new_year" | "lunar_new_year" => Some("Lunar New Year"),
            "mix.seasonal.valentines" | "valentines" => Some("Date Night"),
            "mix.seasonal.carnival" | "carnival" => Some("Carnival Beats"),
            "mix.seasonal.celtic_spring" | "celtic_spring" => Some("Celtic Folk"),
            "mix.seasonal.spring" | "spring" => Some("Spring Awakening"),
            "mix.seasonal.summer" | "summer" => Some("Summer Anthems"),
            "mix.seasonal.summer_solstice" | "summer_solstice" => Some("Midsummer Solstice"),
            "mix.seasonal.late_summer_reset" | "late_summer_reset" => Some("Late Summer Reset"),
            "mix.seasonal.autumn" | "autumn" => Some("Autumn Sessions"),
            "mix.seasonal.halloween" | "halloween" => Some("Darkwave Halloween"),
            "mix.moment.drive" | "drive" => Some("Drive Mix"),
            "mix.moment.morning" | "morning" => Some("Morning Lift"),
            "mix.moment.focus" | "focus" => Some("Deep Focus"),
            "mix.moment.evening_unwind" | "evening_unwind" => Some("Evening Unwind"),
            "mix.moment.dinner_groove" | "dinner_groove" => Some("Dinner Groove"),
            "mix.moment.late_night" | "late_night" => Some("Late Night"),
            "mix.moment.rainy_day" | "rainy_day" => Some("Rainy Day"),
            "mix.moment.snow_day" | "snow_day" => Some("Snow Day"),
            "mix.moment.storm_night" | "storm_night" => Some("Storm & Thunder"),
            "mix.moment.sunny_day" | "sunny_day" => Some("Sunny Vibes"),
            "mix.moment.friday_night" | "friday_night" => Some("Friday Night Warmup"),
            "mix.moment.sunday_morning" | "sunday_morning" => Some("Sunday Morning Coffee"),
            "mix.moment.forgotten_gems" | "forgotten_gems" => Some("Forgotten Gems"),
            "mix.moment.fresh_arrivals" | "fresh_arrivals" => Some("Fresh Arrivals"),
            "mix.moment.decade_80s" | "decade_80s" => Some("80s Wave"),
            "mix.moment.decade_90s" | "decade_90s" => Some("90s Mix"),
            "mix.moment.decade_2000s" | "decade_2000s" => Some("2000s Mix"),
            "mix.rolling.conservative" | "mix.rolling.comfort" | "conservative" | "comfort" => {
                Some("Comfort Zone")
            }
            "mix.rolling.balanced" | "mix.rolling.weekly_radar" | "balanced" | "weekly_radar" => {
                Some("Fresh Picks")
            }
            "mix.rolling.adventurous" | "mix.rolling.deep_cuts" | "adventurous" | "deep_cuts" => {
                Some("Deep Cuts")
            }
            _ => None,
        }
    };

    if let Some(key) = i18n_key.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(title) = lookup(key) {
            return title.to_string();
        }
    }
    if let Some(title) = lookup(slug) {
        return title.to_string();
    }

    let fallback = i18n_key
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|k| k.rsplit('.').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(slug);
    let humanized = humanize_mix_slug(fallback);
    if humanized.is_empty() {
        "Curated Mix".to_string()
    } else {
        humanized
    }
}

/// Converts a snake/kebab/dot mix identifier into a Title Case fallback label.
pub fn humanize_mix_slug(slug: &str) -> String {
    slug.split(['_', '-', '.'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    let mut out = first.to_uppercase().to_string();
                    out.push_str(chars.as_str());
                    out
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Pure structured i18n mix card returned by `GET /api/discovery/contextual-mixes`.
/// Clients render all localized titles, subtitles, ratio badges, and expiry text using
/// `i18n_key` + typed metadata (or `custom_name` / `custom_description` for custom mixes).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextualMixSummary {
    pub slug: String,
    pub kind: ContextualMixKind,
    pub i18n_key: String,
    #[serde(default)]
    pub custom_name: Option<String>,
    #[serde(default)]
    pub custom_description: Option<String>,
    #[serde(default)]
    pub built_in: bool,
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default)]
    pub local_pct: Option<u8>,
    pub owned_pct: u8,
    #[serde(default)]
    pub offline_priority: bool,
    pub exclude_from_core_taste: bool,
    #[serde(default)]
    pub steering: Option<CultureSteering>,
    #[serde(default)]
    pub active_from: Option<String>,
    #[serde(default)]
    pub active_until: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub navidrome_playlist_id: Option<String>,
    /// Live local weather snapshot when this mix was activated by weather conditions.
    #[serde(default)]
    pub weather_badge: Option<WeatherSnapshot>,
    pub track_count: u32,
    pub target_tracks: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextualMixKind {
    Seasonal,
    Moment,
    Rolling,
}

impl ContextualMixKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            ContextualMixKind::Seasonal => "seasonal",
            ContextualMixKind::Moment => "moment",
            ContextualMixKind::Rolling => "rolling",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "seasonal" => Some(ContextualMixKind::Seasonal),
            "moment" => Some(ContextualMixKind::Moment),
            "rolling" => Some(ContextualMixKind::Rolling),
            _ => None,
        }
    }
}

/// Request to create or update a custom contextual mix definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpsertContextualMixRequest {
    pub slug: String,
    pub kind: ContextualMixKind,
    #[serde(default)]
    pub custom_name: Option<String>,
    #[serde(default)]
    pub custom_description: Option<String>,
    /// When `true` (admin only), creates/updates a global mix shared across all users.
    #[serde(default)]
    pub global: bool,
    /// Optional explicit enabled flag; when `None`, preserves `existing.enabled` on edit (or `true` on create).
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub theme_tags: Vec<String>,
    #[serde(default)]
    pub international_queries: Vec<String>,
    #[serde(default)]
    pub calendar_window: Option<CalendarWindow>,
    #[serde(default)]
    pub hour_window: Option<HourWindow>,
    #[serde(default)]
    pub days_of_week: Vec<u8>,
    #[serde(default)]
    pub weather_conditions: Vec<WeatherCondition>,
    #[serde(default)]
    pub always_surfaced: bool,
    #[serde(default)]
    pub countries: Vec<String>,
    #[serde(default)]
    pub owned_ratio: f32,
    #[serde(default)]
    pub default_local_ratio: Option<f32>,
    #[serde(default)]
    pub exclude_from_core_taste: bool,
    #[serde(default)]
    pub offline_priority: bool,
    #[serde(default = "default_target_tracks")]
    pub target_tracks: u32,
    #[serde(default = "default_true")]
    pub sync_to_navidrome: bool,
    #[serde(default)]
    pub selection_rules: Option<MixSelectionRules>,
}

/// Per-user toggle to enable or disable any mix definition without modifying global state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetMixEnabledRequest {
    pub slug: String,
    pub enabled: bool,
}

/// One playable track inside a materialized contextual mix playlist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextualMixTrack {
    pub song_id: String,
    #[serde(default)]
    pub discovery_track_id: Option<String>,
    pub artist: String,
    pub title: String,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub duration_s: Option<u32>,
    pub owned: bool,
    #[serde(default)]
    pub rating: Option<u8>,
    #[serde(default)]
    pub starred: bool,
    #[serde(default)]
    pub reason: Option<CandidateReason>,
    #[serde(default)]
    pub culture_bucket: Option<CultureBucket>,
}

/// Materialized playable playlist for a contextual mix (owned library + staged fresh tracks),
/// plus optional unowned prefetch candidates when fresh slots need filling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextualMixPlaylist {
    pub summary: ContextualMixSummary,
    pub tracks: Vec<ContextualMixTrack>,
    #[serde(default)]
    pub prefetch_candidates: Vec<Candidate>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateCultureProfileRequest {
    #[serde(default)]
    pub home_country: Option<String>,
    #[serde(default)]
    pub preferred_languages: Option<Vec<String>>,
    #[serde(default)]
    pub local_culture_ratio: Option<f32>,
    /// Optional city name for weather-reactive mixes (e.g. `"Lyon"`).
    /// Pass `Some("")` to clear and return to automatic IP geolocation; `None` leaves unchanged.
    #[serde(default)]
    pub weather_city: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteerSeasonalPackRequest {
    pub slug: String,
    pub steering: CultureSteering,
}

/// Request to start an on-demand Seed Radio from a track or artist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeedRadioRequest {
    #[serde(default)]
    pub seed_song_id: Option<String>,
    #[serde(default)]
    pub seed_artist: Option<String>,
    #[serde(default)]
    pub seed_track: Option<String>,
    #[serde(default)]
    pub seed_artist_mbid: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Owned library track returned immediately by Seed Radio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeedRadioTrack {
    pub song_id: String,
    pub title: String,
    pub artist: String,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub duration: Option<u32>,
    #[serde(default)]
    pub rating: Option<u8>,
    #[serde(default)]
    pub starred: bool,
    pub reason: CandidateReason,
}

/// Response from `POST /api/radio/start`: owned matches play immediately while unowned
/// `prefetch_candidates` can be fast-acquired seamlessly in the background.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeedRadioResponse {
    pub radio_id: String,
    pub seed_label: String,
    pub owned_tracks: Vec<SeedRadioTrack>,
    pub prefetch_candidates: Vec<Candidate>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT_0_9_0: &str = r#"{"profile_source":"ListenBrainz+Last.fm","profile_summary":{"genre_count":2,"top_genres":["rock","pop"],"obscurity_score":0.4,"repeat_ratio":0.7,"freshness_half_life_days":90.0,"momentum_artists":["Khruangbin"],"comfort_tags":1,"exploration_tags":1},"pipeline_reports":[{"name":"lastfm_pipeline","signals":[{"name":"lfm_track_graph","candidates_produced":3,"top_candidates":[{"artist":"Muse","track":"Hysteria","score":0.9,"genre":"rock","listeners":3000000}]}],"total_candidates":3,"mbid_failures":0}],"blend_summary":{"sources":1,"total_after_blend":3,"cross_source_matches":0,"top_blended":[]},"freshness_summary":{"known_artists_penalized":1,"total_candidates":3,"penalty_factor":0.67,"new_release_boosted":0},"diversifier_summary":{"popularity_penalized":0,"artist_cap_skipped":0,"genre_quota_skipped":0,"exploration_backfilled":0,"effective_exploration_budget":0.2,"unique_artists":3,"unique_genres":1,"top_selected":[]},"final_count":3,"duration_secs":12.5}"#;

    #[test]
    fn a_report_stored_by_0_9_0_still_loads() {
        let r: EngineReport = serde_json::from_str(REPORT_0_9_0).unwrap();
        assert_eq!(
            (
                r.state,
                r.sources.len(),
                r.source_errors.len(),
                r.final_count
            ),
            (EngineState::Recommended, 0, 0, 3)
        );
        assert!(r.to_text().contains("lfm_track_graph"));
    }

    #[test]
    fn a_report_names_its_sources_its_failures_and_the_empty_state() {
        let mut errors = SourceErrors::default();
        errors.record(
            "ListenBrainz Labs",
            "ListenBrainz Labs did not answer: timeout",
        );
        errors.record("ListenBrainz Labs", "other");
        let r = EngineReport {
            sources: vec!["Navidrome".into(), "ListenBrainz similar artists".into()],
            source_errors: errors.into_vec(),
            ..Default::default()
        };
        let text = r.to_text();
        assert!(
            text.contains("Sources: Navidrome, ListenBrainz similar artists"),
            "{text}"
        );
        assert!(
            text.contains(
                "ListenBrainz Labs failed 2 times: ListenBrainz Labs did not answer: timeout"
            ),
            "{text}"
        );
        let empty = EngineReport {
            state: EngineState::NoListeningData,
            ..Default::default()
        };
        assert!(empty.to_text().contains(NO_LISTENING_DATA));
    }

    #[test]
    fn known_music_knows_an_artist_by_name_or_mbid_and_a_track_by_key_or_recording() {
        let mut k = KnownMusic::default();
        k.add_track(&TrackRef {
            artist: ArtistRef {
                name: "フィッシュマンズ".into(),
                mbid: Some("D0F25F16-7782-42E8-8047-C16A1CB2B450".into()),
            },
            title: "ナイトクルージング".into(),
            mbid: Some("r1".into()),
        });
        assert!(k.knows_artist(&ArtistRef {
            name: "Fishmans".into(),
            mbid: Some("d0f25f16-7782-42e8-8047-c16a1cb2b450".into())
        }));
        assert!(!k.knows_artist(&ArtistRef {
            name: "Fishmans".into(),
            mbid: None
        }));
        assert!(k.knows_track(&TrackRef {
            artist: ArtistRef {
                name: "x".into(),
                mbid: None
            },
            title: "y".into(),
            mbid: Some("R1".into())
        }));
        assert!(k.knows_track(&TrackRef {
            artist: ArtistRef {
                name: "フィッシュマンズ".into(),
                mbid: None
            },
            title: "ナイトクルージング".into(),
            mbid: None
        }));
    }

    #[test]
    fn source_errors_keep_the_first_message_per_source() {
        let mut e = SourceErrors::default();
        e.record("Deezer", "first");
        e.record("Deezer", "second");
        e.record("MusicBrainz", "mb");
        assert_eq!(
            e.into_vec(),
            vec![
                SourceError {
                    source: "Deezer".into(),
                    message: "first".into(),
                    failures: 2
                },
                SourceError {
                    source: "MusicBrainz".into(),
                    message: "mb".into(),
                    failures: 1
                },
            ]
        );
    }

    #[test]
    fn a_reports_errors_group_by_source_adding_up_their_failures() {
        let error = |source: &str, message: &str, failures| SourceError {
            source: source.into(),
            message: message.into(),
            failures,
        };
        let grouped = SourceErrors::from(vec![
            error("Deezer", "first", 2),
            error("MusicBrainz", "mb", 1),
            error("Deezer", "second", 3),
        ]);
        assert_eq!(
            grouped.into_vec(),
            [error("Deezer", "first", 5), error("MusicBrainz", "mb", 1)]
        );
    }

    #[test]
    fn a_library_track_is_known_however_a_source_accents_its_names() {
        let mut known = KnownMusic::default();
        known.add_track(&TrackRef {
            artist: ArtistRef {
                name: "SKÁLD".into(),
                mbid: None,
            },
            title: "Ódinn".into(),
            mbid: None,
        });
        let unaccented = TrackRef {
            artist: ArtistRef {
                name: "Skald".into(),
                mbid: None,
            },
            title: "Odinn".into(),
            mbid: None,
        };
        assert!(known.knows_artist(&unaccented.artist));
        assert!(known.knows_track(&unaccented));
        assert_eq!(
            CandidateSet::key("SKÁLD", "Ódinn"),
            CandidateSet::key("SKALD", "Odinn")
        );
    }

    #[test]
    fn fold_name_keeps_the_punctuation_that_names_another_act() {
        assert_ne!(fold_name("Fishmans+"), fold_name("Fishmans"));
        assert_eq!(fold_name("  Guns N’ Roses "), "guns n' roses");
        assert_eq!(fold_name("Fèqadu  Amdè‐Mesqel"), "fèqadu amdè-mesqel");
    }

    #[test]
    fn canonical_mix_title_is_neutral_and_consistent() {
        assert_eq!(
            canonical_mix_title("winter_holiday", Some("mix.seasonal.winter_holiday"), None),
            "Christmas Mix"
        );
        assert_eq!(
            canonical_mix_title("summer", Some("mix.seasonal.summer"), None),
            "Summer Anthems"
        );
        assert_eq!(
            canonical_mix_title("halloween", None, None),
            "Darkwave Halloween"
        );
        assert_eq!(
            canonical_mix_title("late_night", Some("mix.moment.late_night"), None),
            "Late Night"
        );
        assert_eq!(
            canonical_mix_title("rolling", Some("mix.rolling.balanced"), None),
            "Fresh Picks"
        );
        assert_eq!(
            canonical_mix_title("winter_holiday", Some("mix.seasonal.winter_holiday"), Some("Custom Xmas")),
            "Custom Xmas"
        );
        assert_eq!(
            canonical_mix_title("midsummer_night", None, None),
            "Midsummer Night"
        );
        assert_eq!(
            canonical_mix_title("foo_bar", Some("custom_vibes"), None),
            "Custom Vibes"
        );
        assert_eq!(
            canonical_mix_title("  late_coding  ", None, None),
            "Late Coding"
        );
        assert_eq!(canonical_mix_title("___", Some("..."), None), "Curated Mix");
    }
}
