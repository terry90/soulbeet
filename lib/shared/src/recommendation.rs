use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt::Display;

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
    #[serde(default)]
    pub artist_mbid: Option<String>,
}

/// A set of candidates from a single pipeline
#[derive(Debug, Clone, Default)]
pub struct CandidateSet {
    pub candidates: std::collections::HashMap<String, Candidate>,
}

/// Normalize a string for cross-source matching.
/// Lowercases, strips non-alphanumeric characters (keeps whitespace),
/// then collapses whitespace. Handles cases like "don't" vs "dont",
/// "AC/DC" vs "ACDC", and whitespace differences between Last.fm
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
        self.artist_names.insert(normalize_for_matching(&a.name));
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
        self.artist_names.contains(&normalize_for_matching(&a.name))
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

#[derive(Debug, Clone, Default)]
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineState {
    #[default]
    Recommended,
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
            normalize_for_matching(artist),
            normalize_for_matching(track)
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
    pub profile_source: String,
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
            sources: vec!["Navidrome".into(), "catalog".into()],
            source_errors: errors.into_vec(),
            ..Default::default()
        };
        let text = r.to_text();
        assert!(text.contains("Sources: Navidrome, catalog"), "{text}");
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
    fn fold_name_keeps_the_punctuation_that_names_another_act() {
        assert_ne!(fold_name("Fishmans+"), fold_name("Fishmans"));
        assert_eq!(fold_name("  Guns N’ Roses "), "guns n' roses");
        assert_eq!(fold_name("Fèqadu  Amdè‐Mesqel"), "fèqadu amdè-mesqel");
    }
}
