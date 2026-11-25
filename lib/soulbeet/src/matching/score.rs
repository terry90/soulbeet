//! A group's score, on the one scale the picker compares every backend's groups on.

/// A file scored under this is not a match.
pub const MIN_SCORE_THRESHOLD: f64 = 0.6;

/// A group's score from its files' mean match score, the share of the wanted tracks it has, and
/// its files' mean quality score.
pub fn group_score(mean_match: f64, completeness: f64, mean_quality: f64) -> f64 {
    (mean_match * 0.3) + (completeness * 0.3) + (mean_quality * 0.4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_weighs_match_and_completeness_three_tenths_and_quality_four() {
        for (m, c, q) in [(1.0, 1.0, 1.0), (0.8, 0.5, 0.95), (0.6, 1.0 / 3.0, 0.25)] {
            assert_eq!(group_score(m, c, q), (m * 0.3) + (c * 0.3) + (q * 0.4));
        }
        assert!((group_score(1.0, 1.0, 1.0) - 1.0).abs() < 1e-12);
    }
}
