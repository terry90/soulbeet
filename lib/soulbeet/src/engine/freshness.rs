use chrono::Datelike;
use tracing::info;

use shared::recommendation::{CandidateSet, FreshnessSummary, ProfileConfig};

/// Boosts candidates released this year by the profile's new-release boost,
/// and those of last year by half as much.
pub fn apply_freshness(candidates: &mut CandidateSet, config: &ProfileConfig) -> FreshnessSummary {
    let mut boosted = 0usize;
    let current_year = chrono::Utc::now().year() as u16;

    for candidate in candidates.candidates.values_mut() {
        if let Some(year) = candidate.release_year {
            let age = current_year.saturating_sub(year);
            if age == 0 {
                candidate.score *= config.new_release_boost;
                boosted += 1;
            } else if age <= 1 {
                candidate.score *= 1.0 + (config.new_release_boost - 1.0) * 0.5;
                boosted += 1;
            }
        }
    }

    info!("Freshness pass: boosted {} new releases", boosted);

    FreshnessSummary {
        total_candidates: candidates.len(),
        new_release_boosted: boosted,
    }
}
