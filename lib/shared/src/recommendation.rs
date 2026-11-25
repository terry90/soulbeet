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
    /// Consumer-facing explanation pill (e.g. "FRENCH NOËL PICK", "BECAUSE YOU LIKE ELECTRO").
    #[serde(default)]
    pub reason_label: Option<String>,
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

/// Explainable reason metadata attached to a recommendation candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateReason {
    pub label: String,
    #[serde(default)]
    pub bucket: Option<CultureBucket>,
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

/// Calendar-driven seasonal pack kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeasonalKind {
    Noel,
    NewYearsEve,
    Summer,
    Halloween,
    Autumn,
    Spring,
}

impl SeasonalKind {
    pub const ALL: [SeasonalKind; 6] = [
        SeasonalKind::Noel,
        SeasonalKind::NewYearsEve,
        SeasonalKind::Summer,
        SeasonalKind::Halloween,
        SeasonalKind::Autumn,
        SeasonalKind::Spring,
    ];

    pub const fn slug(self) -> &'static str {
        match self {
            SeasonalKind::Noel => "noel",
            SeasonalKind::NewYearsEve => "nye",
            SeasonalKind::Summer => "summer",
            SeasonalKind::Halloween => "halloween",
            SeasonalKind::Autumn => "autumn",
            SeasonalKind::Spring => "spring",
        }
    }

    pub fn from_slug(slug: &str) -> Option<Self> {
        let normalized = slug.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|k| k.slug() == normalized)
    }

    /// Returns the seasonal pack active for a `(month, day)` calendar date (1-based).
    pub fn active_for(month: u32, day: u32) -> Option<Self> {
        match (month, day) {
            (11, 20..=30) | (12, 1..=27) => Some(SeasonalKind::Noel),
            (12, 28..=31) | (1, 1..=2) => Some(SeasonalKind::NewYearsEve),
            (3, 20..=31) | (4, 1..=30) | (5, 1..=31) => Some(SeasonalKind::Spring),
            (6, 1..=30) | (7, 1..=31) | (8, 1..=31) => Some(SeasonalKind::Summer),
            (10, 15..=31) | (11, 1) => Some(SeasonalKind::Halloween),
            (9, 22..=30) | (10, 1..=14) | (11, 2..=19) => Some(SeasonalKind::Autumn),
            _ => None,
        }
    }

    pub fn localized_title(self, country: Option<&str>) -> String {
        let is_fr = country.is_some_and(|c| c.eq_ignore_ascii_case("FR"));
        match self {
            SeasonalKind::Noel if is_fr => "Noël à la Française".to_string(),
            SeasonalKind::Noel => "Holiday Classics & Gems".to_string(),
            SeasonalKind::NewYearsEve if is_fr => "Réveillon Bangers".to_string(),
            SeasonalKind::NewYearsEve => "New Year's Eve Party".to_string(),
            SeasonalKind::Summer if is_fr => "Soleil & Été".to_string(),
            SeasonalKind::Summer => "Summer Anthems".to_string(),
            SeasonalKind::Halloween => "Halloween Dark Mode".to_string(),
            SeasonalKind::Autumn if is_fr => "Automne Cosy".to_string(),
            SeasonalKind::Autumn => "Autumn Cozy".to_string(),
            SeasonalKind::Spring if is_fr => "Printemps — Fresh Cuts".to_string(),
            SeasonalKind::Spring => "Spring Fresh Cuts".to_string(),
        }
    }

    pub fn subtitle(self, country: Option<&str>) -> String {
        let is_fr = country.is_some_and(|c| c.eq_ignore_ascii_case("FR"));
        match self {
            SeasonalKind::Noel if is_fr => {
                "French holiday staples blended with international winter classics".to_string()
            }
            SeasonalKind::Noel => {
                "Festive local favorites and international holiday classics".to_string()
            }
            SeasonalKind::NewYearsEve => {
                "High-energy dance, pop, and party anthems for midnight".to_string()
            }
            SeasonalKind::Summer => {
                "Sun-soaked local hits, festival anthems, and warm grooves".to_string()
            }
            SeasonalKind::Halloween => {
                "Darksynth, post-punk, and spooky classics tuned to your taste".to_string()
            }
            SeasonalKind::Autumn => {
                "Warm acoustic, indie, and mellow folk for crisp evenings".to_string()
            }
            SeasonalKind::Spring => {
                "Fresh releases and upbeat indie cuts across your orbit".to_string()
            }
        }
    }

    pub fn active_window_label(self) -> &'static str {
        match self {
            SeasonalKind::Noel => "Nov 20 – Dec 27",
            SeasonalKind::NewYearsEve => "Dec 28 – Jan 2",
            SeasonalKind::Summer => "Jun 1 – Aug 31",
            SeasonalKind::Halloween => "Oct 15 – Nov 1",
            SeasonalKind::Autumn => "Sep 22 – Nov 19",
            SeasonalKind::Spring => "Mar 20 – May 31",
        }
    }

    pub fn expires_label(self) -> &'static str {
        match self {
            SeasonalKind::Noel => "Auto-cleans Jan 5 unless kept",
            SeasonalKind::NewYearsEve => "Auto-cleans Jan 8 unless kept",
            SeasonalKind::Summer => "Auto-cleans Sep 7 unless kept",
            SeasonalKind::Halloween => "Auto-cleans Nov 8 unless kept",
            SeasonalKind::Autumn => "Auto-cleans Nov 26 unless kept",
            SeasonalKind::Spring => "Auto-cleans Jun 7 unless kept",
        }
    }

    /// Primary ListenBrainz / MusicBrainz theme tag for this seasonal pack.
    pub fn primary_tag(self) -> &'static str {
        match self {
            SeasonalKind::Noel => "christmas",
            SeasonalKind::NewYearsEve => "dance",
            SeasonalKind::Summer => "summer",
            SeasonalKind::Halloween => "gothic rock",
            SeasonalKind::Autumn => "acoustic",
            SeasonalKind::Spring => "indie pop",
        }
    }
}

/// Complete specification for a culturally blended seasonal pack.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeasonalPackSpec {
    pub slug: String,
    pub kind: SeasonalKind,
    pub title: String,
    pub subtitle: String,
    pub steering: CultureSteering,
    pub local_ratio: f32,
    pub exclude_from_core_taste: bool,
    pub active_window: String,
    pub expires_label: String,
}

impl SeasonalPackSpec {
    pub fn for_kind(
        kind: SeasonalKind,
        culture: &UserCultureProfile,
        steering: CultureSteering,
    ) -> Self {
        let country = culture.effective_country();
        Self {
            slug: kind.slug().to_string(),
            kind,
            title: kind.localized_title(country),
            subtitle: kind.subtitle(country),
            steering,
            local_ratio: steering.effective_ratio(culture.local_culture_ratio),
            exclude_from_core_taste: true,
            active_window: kind.active_window_label().to_string(),
            expires_label: kind.expires_label().to_string(),
        }
    }
}

/// Contextual Moment Mix kinds blending ~75% owned library + ~25% fresh Discovery tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MomentKind {
    Drive,
    Morning,
    Focus,
    LateNight,
}

impl MomentKind {
    pub const ALL: [MomentKind; 4] = [
        MomentKind::Drive,
        MomentKind::Morning,
        MomentKind::Focus,
        MomentKind::LateNight,
    ];

    pub const fn slug(self) -> &'static str {
        match self {
            MomentKind::Drive => "drive",
            MomentKind::Morning => "morning",
            MomentKind::Focus => "focus",
            MomentKind::LateNight => "late_night",
        }
    }

    pub const fn title(self) -> &'static str {
        match self {
            MomentKind::Drive => "Drive Mix",
            MomentKind::Morning => "Morning Boost",
            MomentKind::Focus => "Focus Flow",
            MomentKind::LateNight => "Late Night",
        }
    }

    pub const fn subtitle(self) -> &'static str {
        match self {
            MomentKind::Drive => "Consistent energy, offline-ready favorites & fresh road picks",
            MomentKind::Morning => "Upbeat openers to start the day with momentum",
            MomentKind::Focus => "Steady grooves and instrumental textures for deep work",
            MomentKind::LateNight => "Warm downtempo, atmospheric cuts for after dark",
        }
    }

    pub fn active_for_hour(hour: u32) -> Self {
        match hour {
            6..=10 => MomentKind::Morning,
            11..=17 => MomentKind::Focus,
            22..=23 | 0..=4 => MomentKind::LateNight,
            _ => MomentKind::Drive,
        }
    }

    pub fn mood_tags(self) -> &'static [&'static str] {
        match self {
            MomentKind::Drive => &["indie rock", "electronic", "synthpop"],
            MomentKind::Morning => &["pop", "indie pop", "funk"],
            MomentKind::Focus => &["ambient", "downtempo", "instrumental"],
            MomentKind::LateNight => &["trip hop", "dream pop", "chillout"],
        }
    }

    pub const fn owned_ratio(self) -> f32 {
        0.75
    }
}

/// Unified mix card returned by `GET /api/discovery/contextual-mixes`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextualMixSummary {
    pub slug: String,
    pub kind: ContextualMixKind,
    pub title: String,
    pub subtitle: String,
    pub badge: String,
    pub exclude_from_core_taste: bool,
    #[serde(default)]
    pub steering: Option<CultureSteering>,
    #[serde(default)]
    pub local_ratio: Option<f32>,
    #[serde(default)]
    pub expires_label: Option<String>,
    pub track_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextualMixKind {
    Seasonal,
    Moment,
    Rolling,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateCultureProfileRequest {
    #[serde(default)]
    pub home_country: Option<String>,
    #[serde(default)]
    pub preferred_languages: Option<Vec<String>>,
    #[serde(default)]
    pub local_culture_ratio: Option<f32>,
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
    pub reason_label: String,
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
}
