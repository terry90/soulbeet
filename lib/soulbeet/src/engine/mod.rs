pub mod blender;
pub mod catalog;
pub mod deezer_radio;
pub mod diversifier;
pub mod freshness;
pub mod listenbrainz_account;
pub mod taste;
#[cfg(test)]
pub(crate) mod test_support;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use crate::traits::CandidateGenerator;
use crate::MusicKnowledge;
use shared::recommendation::{
    fold_name, Candidate, CandidateSet, CandidateSnapshot, EngineReport, EngineState,
    PipelineReport, ProfileConfig, ProfileSummary, SignalReport, SourceErrors, TrackRef,
    UserMusicProfile,
};

pub use taste::{build_profile, ProfileBuild};

/// Keeps one signal from crowding out the others in the blend.
pub const MAX_CANDIDATES_PER_SIGNAL: usize = 500;
/// One hour: a generator still running then is given up on for this run.
const GENERATOR_TIMEOUT: Duration = Duration::from_secs(3600);
/// Facts are read for this many candidates per one wanted, the best scored first.
const ENRICHED_PER_WANTED: usize = 3;

/// Keeps the best-scored `MAX_CANDIDATES_PER_SIGNAL` candidates of a signal.
pub(crate) fn cap_signal(set: &mut CandidateSet) {
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

/// A signal's count and its three best candidates, for the run report.
pub(crate) fn signal_report(name: &str, set: &CandidateSet) -> SignalReport {
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

/// Caps each signal, reports it and merges it into one set, signals in order.
pub(crate) fn merge_signals(
    signals: Vec<(&str, CandidateSet)>,
) -> (CandidateSet, Vec<SignalReport>) {
    let mut candidates = CandidateSet::new();
    let mut reports = Vec::new();
    for (name, mut set) in signals {
        cap_signal(&mut set);
        reports.push(signal_report(name, &set));
        for candidate in set.into_vec() {
            candidates.insert(candidate);
        }
    }
    (candidates, reports)
}

/// A candidate for a track one signal of a generator found.
pub(crate) fn track_candidate(
    track: TrackRef,
    score: f64,
    signal: &str,
    source: &str,
) -> Candidate {
    Candidate {
        artist: track.artist.name,
        track: track.title,
        album: None,
        mbid: track.mbid,
        score,
        signals: vec![signal.to_string()],
        source: source.to_string(),
        artist_listener_count: None,
        primary_genre: None,
        release_year: None,
        artist_mbid: track.artist.mbid,
    }
}

/// Recommends `target_count` tracks from a built profile.
///
/// 1. Each generator runs; one that fails or times out is reported and the others go on.
/// 2. The generators' candidates are blended by their weights.
/// 3. The best candidates get their release year, popularity and genre from the knowledge.
/// 4. New releases are boosted.
/// 5. The diversifier picks the final list.
///
/// A profile without seeds runs no generator. The report says there is no listening data only
/// when every history answered, since one that failed may hold it.
pub async fn recommend(
    build: &ProfileBuild,
    generators: &[Arc<dyn CandidateGenerator>],
    knowledge: &dyn MusicKnowledge,
    config: &ProfileConfig,
    target_count: usize,
) -> (Vec<Candidate>, EngineReport) {
    let start = Instant::now();
    let profile = &build.profile;
    let mut errors = build.errors.clone();
    let mut report = EngineReport {
        profile_summary: profile_summary(profile),
        sources: build.sources.clone(),
        ..Default::default()
    };

    if !profile.has_seeds() {
        info!("No listening data to recommend from");
        if errors.is_empty() {
            report.state = EngineState::NoListeningData;
        }
        report.source_errors = errors.into_vec();
        report.duration_secs = start.elapsed().as_secs_f64();
        return (Vec::new(), report);
    }

    info!(
        "Starting recommendation: {} generators, target {}",
        generators.len(),
        target_count
    );
    let mut sets = Vec::new();
    for generator in generators {
        let name = generator.name();
        let Ok(output) =
            tokio::time::timeout(GENERATOR_TIMEOUT, generator.generate(profile, config)).await
        else {
            warn!("Generator '{name}' timed out after 1 h");
            errors.record(name, format!("{name} timed out after 1 h"));
            continue;
        };
        info!(
            "Generator '{}' produced {} candidates",
            name,
            output.candidates.len()
        );
        errors.extend(output.errors);
        report.pipeline_reports.push(PipelineReport {
            name: name.to_string(),
            signals: output.signals,
            total_candidates: output.candidates.len(),
            mbid_failures: output.mbid_failures,
        });
        if !output.candidates.is_empty() {
            report.sources.push(name.to_string());
            sets.push((name, generator.weight(), output.candidates));
        }
    }

    let (mut blended, blend_summary) = blender::blend(sets, config);
    report.blend_summary = blend_summary;

    enrich(
        &mut blended,
        knowledge,
        target_count * ENRICHED_PER_WANTED,
        &mut errors,
    )
    .await;

    report.freshness_summary = freshness::apply_freshness(&mut blended, config);

    let (result, diversifier_summary) =
        diversifier::diversify(blended, profile, config, target_count);
    info!("Final recommendation: {} tracks", result.len());
    report.diversifier_summary = diversifier_summary;

    report.source_errors = errors.into_vec();
    for e in &report.source_errors {
        warn!("{} failed {} times: {}", e.source, e.failures, e.message);
    }
    report.final_count = result.len();
    report.duration_secs = start.elapsed().as_secs_f64();
    (result, report)
}

fn profile_summary(profile: &UserMusicProfile) -> ProfileSummary {
    ProfileSummary {
        genre_count: profile.genre_distribution.len(),
        top_genres: profile
            .genre_distribution
            .iter()
            .take(5)
            .map(|t| t.name.clone())
            .collect(),
        obscurity_score: profile.obscurity_score,
        repeat_ratio: profile.repeat_ratio,
        momentum_artists: profile
            .momentum_artists
            .iter()
            .map(|a| a.name.clone())
            .collect(),
        comfort_tags: profile.tag_comfort_zone.len(),
        exploration_tags: profile.tag_exploration_zone.len(),
    }
}

/// Fills the release year, artist MBID, listener count and genre of the
/// `count` best candidates from the knowledge. A failed read is recorded and
/// leaves the candidates as they are. The release year needs a recording MBID,
/// so a candidate whose source names the artist only (Deezer) gets none.
async fn enrich(
    candidates: &mut CandidateSet,
    knowledge: &dyn MusicKnowledge,
    count: usize,
    errors: &mut SourceErrors,
) {
    let mut keys: Vec<String> = candidates.candidates.keys().cloned().collect();
    keys.sort_by(|a, b| {
        candidates.candidates[b]
            .score
            .total_cmp(&candidates.candidates[a].score)
    });
    let keys: HashSet<String> = keys.into_iter().take(count).collect();
    let mut best: Vec<&mut Candidate> = candidates
        .candidates
        .iter_mut()
        .filter(|(key, _)| keys.contains(*key))
        .map(|(_, c)| c)
        .collect();

    let recordings = distinct(best.iter().filter_map(|c| c.mbid.clone()));
    if !recordings.is_empty() {
        match knowledge.recordings(&recordings).await {
            Ok(facts) => {
                for c in best.iter_mut() {
                    let Some(f) = c.mbid.as_ref().and_then(|m| facts.get(m)) else {
                        continue;
                    };
                    c.release_year = c.release_year.or(f.first_release_year);
                    if c.artist_mbid.is_none() {
                        c.artist_mbid.clone_from(&f.artist.mbid);
                    }
                }
            }
            Err(e) => errors.record(e.service(), &e),
        }
    }

    // A source that names the artist only, like Deezer, needs the MBID looked up
    // to get a listener count and a genre.
    let mut resolver = Resolver::new(knowledge);
    for c in best.iter_mut() {
        resolver.fill(&mut c.artist_mbid, &c.artist, errors).await;
    }

    let artists = distinct(best.iter().filter_map(|c| c.artist_mbid.clone()));
    if artists.is_empty() {
        return;
    }
    match knowledge.artist_popularity(&artists).await {
        Ok(users) => {
            for c in best.iter_mut() {
                if let Some(&n) = c.artist_mbid.as_ref().and_then(|m| users.get(m)) {
                    c.artist_listener_count = Some(n);
                }
            }
        }
        Err(e) => errors.record(e.service(), &e),
    }
    match knowledge.artist_tags(&artists).await {
        Ok(tags) => {
            for c in best.iter_mut() {
                let first = c
                    .artist_mbid
                    .as_ref()
                    .and_then(|m| tags.get(m))
                    .and_then(|t| t.first());
                if let Some(tag) = first {
                    c.primary_genre = Some(tag.name.clone());
                }
            }
        }
        Err(e) => errors.record(e.service(), &e),
    }
}

/// Asks the knowledge once per name and run for the artist MBIDs histories
/// and generators leave out.
pub(crate) struct Resolver<'a> {
    knowledge: &'a dyn MusicKnowledge,
    answers: HashMap<String, Option<String>>,
    failed: HashSet<String>,
    pub(crate) failures: usize,
}

impl<'a> Resolver<'a> {
    pub(crate) fn new(knowledge: &'a dyn MusicKnowledge) -> Self {
        Self {
            knowledge,
            answers: HashMap::new(),
            failed: HashSet::new(),
            failures: 0,
        }
    }

    pub(crate) async fn fill(
        &mut self,
        mbid: &mut Option<String>,
        name: &str,
        errors: &mut SourceErrors,
    ) {
        if mbid.is_some() {
            return;
        }
        let key = fold_name(name);
        if self.failed.contains(&key) {
            return;
        }
        if let Some(answer) = self.answers.get(&key) {
            mbid.clone_from(answer);
            return;
        }
        match self.knowledge.resolve_artist(name).await {
            Ok(answer) => {
                mbid.clone_from(&answer);
                self.answers.insert(key, answer);
            }
            // A failed lookup is not "no such artist"; it is only skipped for this run.
            Err(e) => {
                self.failures += 1;
                errors.record(e.service(), &e);
                self.failed.insert(key);
            }
        }
    }
}

/// The values in their first-seen order, each once.
fn distinct(values: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values.filter(|v| seen.insert(v.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use shared::recommendation::{EngineState, RecordingFacts, SourceErrors};

    fn seeded() -> ProfileBuild {
        ProfileBuild {
            profile: UserMusicProfile {
                seed_artists: vec![played("A", None, 5)],
                ..Default::default()
            },
            sources: vec!["Navidrome".into()],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn an_empty_profile_runs_no_generator_and_says_so() {
        let (got, report) = recommend(
            &ProfileBuild {
                sources: vec!["Navidrome".into()],
                ..Default::default()
            },
            &[fixed("catalog", 0.4, &[("B", "b1", 1.0)])],
            &FakeKnowledge::default(),
            &balanced(),
            50,
        )
        .await;
        assert!(got.is_empty());
        assert_eq!(
            (report.state, report.sources.clone()),
            (EngineState::NoListeningData, vec!["Navidrome".to_string()])
        );
        assert!(report.pipeline_reports.is_empty());
    }

    #[tokio::test]
    async fn an_empty_profile_with_a_failed_history_reports_the_failure_not_missing_data() {
        let mut errors = SourceErrors::default();
        errors.record("ListenBrainz", "ListenBrainz answered with a browser check");
        let (got, report) = recommend(
            &ProfileBuild {
                errors,
                ..Default::default()
            },
            &[fixed("catalog", 0.4, &[("B", "b1", 1.0)])],
            &FakeKnowledge::default(),
            &balanced(),
            50,
        )
        .await;
        assert!(got.is_empty());
        assert_eq!(report.state, EngineState::Recommended);
        assert_eq!(
            (report.source_errors.iter())
                .map(|e| e.source.as_str())
                .collect::<Vec<_>>(),
            ["ListenBrainz"]
        );
        assert!(report.pipeline_reports.is_empty());
    }

    #[tokio::test]
    async fn one_failing_source_does_not_sink_the_run() {
        let mut errors = SourceErrors::default();
        errors.record("ListenBrainz", "ListenBrainz is unavailable: down");
        let account: Arc<dyn CandidateGenerator> = Arc::new(FixedGenerator {
            name: "listenbrainz_account",
            weight: 0.2,
            candidates: vec![],
            errors,
        });
        let catalog = fixed(
            "catalog",
            0.4,
            &[("B", "b1", 1.0), ("C", "c1", 0.8), ("D", "d1", 0.6)],
        );
        let (got, report) = recommend(
            &seeded(),
            &[catalog, account],
            &FakeKnowledge::default(),
            &balanced(),
            50,
        )
        .await;
        assert_eq!(got.len(), 3);
        assert_eq!(report.sources, ["Navidrome", "catalog"]);
        assert_eq!(report.source_errors[0].source, "ListenBrainz");
        assert_eq!(report.state, EngineState::Recommended);
    }

    #[tokio::test]
    async fn release_years_and_popularity_come_from_the_knowledge() {
        let mut found = candidate("catalog", "B", "b1", 1.0);
        found.mbid = Some("r1".into());
        let catalog: Arc<dyn CandidateGenerator> = Arc::new(FixedGenerator {
            name: "catalog",
            weight: 0.4,
            candidates: vec![found],
            errors: SourceErrors::default(),
        });
        let knowledge = FakeKnowledge {
            recordings: [(
                "r1",
                RecordingFacts {
                    title: "b1".into(),
                    artist: artist("B", Some("b")),
                    first_release_year: Some(2026),
                },
            )]
            .into(),
            users: [("b", 900)].into(),
            tags: [("b", vec![tag("shoegaze", 1.0)])].into(),
            ..Default::default()
        };
        let (got, report) = recommend(&seeded(), &[catalog], &knowledge, &balanced(), 50).await;
        let c = &got[0];
        assert_eq!(
            (
                c.release_year,
                c.artist_mbid.as_deref(),
                c.artist_listener_count,
                c.primary_genre.as_deref()
            ),
            (Some(2026), Some("b"), Some(900), Some("shoegaze"))
        );
        assert!(report.source_errors.is_empty());
    }

    #[tokio::test]
    async fn a_candidate_without_mbids_is_resolved_by_name_and_penalised_like_the_others() {
        let quiet = ["q1", "q2", "q3", "q4", "q5", "q6", "q7", "q8", "q9"];
        let mut candidates: Vec<Candidate> = quiet
            .iter()
            .map(|m| {
                let mut c = candidate("deezer", m, "t", 1.0);
                c.artist_mbid = Some(m.to_string());
                c
            })
            .collect();
        // Deezer names the artist only.
        candidates.push(candidate("deezer", "Famous", "Hit", 1.0));
        let deezer: Arc<dyn CandidateGenerator> = Arc::new(FixedGenerator {
            name: "deezer",
            weight: 0.4,
            candidates,
            errors: SourceErrors::default(),
        });
        let knowledge = FakeKnowledge {
            mbids: [("famous", "f")].into(),
            users: quiet
                .iter()
                .zip(1..)
                .map(|(m, n)| (*m, n))
                .chain([("f", 1_000_000)])
                .collect(),
            tags: [("f", vec![tag("pop", 1.0)])].into(),
            ..Default::default()
        };
        let (got, report) = recommend(&seeded(), &[deezer], &knowledge, &balanced(), 50).await;
        let famous = got.iter().find(|c| c.artist == "Famous").unwrap();
        let quiet_score = got.iter().find(|c| c.artist == "q1").unwrap().score;
        assert_eq!(
            (
                famous.artist_mbid.as_deref(),
                famous.artist_listener_count,
                famous.primary_genre.as_deref()
            ),
            (Some("f"), Some(1_000_000), Some("pop"))
        );
        assert_eq!(report.diversifier_summary.popularity_penalized, 1);
        assert!(famous.score < quiet_score);
    }

    #[tokio::test]
    async fn a_failed_enrichment_is_reported_and_the_candidates_stay() {
        let knowledge = FakeKnowledge {
            failing: ["artist_popularity".to_string()].into(),
            users: [("b", 900)].into(),
            ..Default::default()
        };
        let mut found = candidate("catalog", "B", "b1", 1.0);
        found.artist_mbid = Some("b".into());
        let catalog: Arc<dyn CandidateGenerator> = Arc::new(FixedGenerator {
            name: "catalog",
            weight: 0.4,
            candidates: vec![found],
            errors: SourceErrors::default(),
        });
        let (got, report) = recommend(&seeded(), &[catalog], &knowledge, &balanced(), 50).await;
        assert_eq!((got.len(), got[0].artist_listener_count), (1, None));
        assert_eq!(report.source_errors[0].source, "ListenBrainz Labs");
    }
}
