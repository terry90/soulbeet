use std::collections::HashMap;

use tracing::info;

use shared::recommendation::{BlendSummary, CandidateSet, CandidateSnapshot, ProfileConfig};

/// Merges the generators' candidate sets into one.
///
/// Each set is scaled so its best candidate scores its generator's weight,
/// the weights shared out over the sets that hold candidates. A candidate
/// several sets found has its scores summed, then the cross-source bonus and
/// a small bonus per distinct signal.
pub fn blend(
    sources: Vec<(&'static str, f64, CandidateSet)>,
    config: &ProfileConfig,
) -> (CandidateSet, BlendSummary) {
    let sources: Vec<_> = sources
        .into_iter()
        .filter(|(_, _, set)| !set.is_empty())
        .collect();
    let total_weight: f64 = sources.iter().map(|(_, weight, _)| weight).sum();
    let source_count = sources.len();

    let mut merged = CandidateSet::new();
    let mut found_in: HashMap<String, usize> = HashMap::new();
    for (name, weight, set) in sources {
        let share = weight / total_weight;
        let max = set.max_score();
        let scale = if max > 0.0 { share / max } else { 0.0 };
        info!(
            "Blending {} candidates from '{}' at {:.2} of the blend",
            set.len(),
            name,
            share
        );
        for (key, mut candidate) in set.candidates {
            candidate.score *= scale;
            *found_in.entry(key).or_default() += 1;
            merged.insert(candidate);
        }
    }

    for (key, candidate) in merged.candidates.iter_mut() {
        if found_in[key] > 1 {
            candidate.score *= config.cross_source_bonus;
            let signals = candidate.signals.len() as f64;
            if signals > 1.0 {
                candidate.score *= 1.0 + 0.1 * signals.ln();
            }
            candidate.source = "blended".to_string();
        }
    }

    let cross_source_matches = found_in.values().filter(|&&n| n > 1).count();
    info!(
        "Blended result: {} candidates ({} cross-source)",
        merged.len(),
        cross_source_matches
    );

    let mut sorted: Vec<_> = merged.candidates.values().collect();
    sorted.sort_by(|a, b| b.score.total_cmp(&a.score));
    let top_blended = sorted
        .iter()
        .take(5)
        .map(|c| CandidateSnapshot::from_candidate(c))
        .collect();

    let summary = BlendSummary {
        sources: source_count,
        total_after_blend: merged.len(),
        cross_source_matches,
        top_blended,
    };

    (merged, summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::test_support::{balanced, candidate};

    fn set(generator: &str, found: &[(&str, f64, &str)]) -> CandidateSet {
        let mut set = CandidateSet::new();
        for (artist, score, signal) in found {
            let mut c = candidate(generator, artist, "t", *score);
            c.signals = vec![signal.to_string()];
            set.insert(c);
        }
        set
    }

    fn score_of(set: &CandidateSet, artist: &str) -> f64 {
        set.candidates[&CandidateSet::key(artist, "t")].score
    }

    #[test]
    fn weights_renormalise_over_the_sources_that_produced_candidates() {
        let (both, summary) = blend(
            vec![
                ("catalog", 0.4, set("catalog", &[("A", 2.0, "artist_hop1")])),
                (
                    "listenbrainz_account",
                    0.2,
                    set("listenbrainz_account", &[("B", 1.0, "lb_troi")]),
                ),
            ],
            &balanced(),
        );
        assert!((score_of(&both, "A") - 0.4 / 0.6).abs() < 1e-9);
        assert!((score_of(&both, "B") - 0.2 / 0.6).abs() < 1e-9);
        assert_eq!((summary.sources, summary.cross_source_matches), (2, 0));
        let (alone, _) = blend(
            vec![("catalog", 0.4, set("catalog", &[("A", 2.0, "artist_hop1")]))],
            &balanced(),
        );
        assert!((score_of(&alone, "A") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_candidate_both_sources_found_gets_the_cross_source_bonus() {
        let (got, summary) = blend(
            vec![
                ("catalog", 0.4, set("catalog", &[("A", 1.0, "artist_hop1")])),
                ("deezer", 0.4, set("deezer", &[("A", 1.0, "dz_radio")])),
            ],
            &balanced(),
        );
        let a = &got.candidates[&CandidateSet::key("A", "t")];
        let expected = (0.5 + 0.5) * 1.3 * (1.0 + 0.1 * 2f64.ln());
        assert!((a.score - expected).abs() < 1e-9, "{}", a.score);
        assert_eq!(a.source, "blended");
        assert_eq!(a.signals, ["artist_hop1", "dz_radio"]);
        assert_eq!(summary.cross_source_matches, 1);
    }
}
