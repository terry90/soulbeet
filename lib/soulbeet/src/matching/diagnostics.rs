//! Bounded pre-enqueue rejection diagnostics shared across matchers and search pipelines.

use super::duration::duration_score;
use super::path::{Ranked, WantedTrack};
use super::version::version_veto;
use shared::download::{RejectedCandidateSummary, SearchStats, MAX_TOP_REJECTED};
use std::collections::BTreeMap;
use std::path::Path;

pub const REASON_VERSION_VETO: &str = "version_veto";
pub const REASON_DURATION_DELTA: &str = "duration_delta";
pub const REASON_ARTIST_GATE: &str = "artist_gate";
pub const REASON_TITLE_MISMATCH: &str = "title_mismatch";
pub const REASON_BELOW_THRESHOLD: &str = "below_threshold";
pub const REASON_FORMAT_FILTERED: &str = "format_filtered";
pub const REASON_LOCKED_ONLY: &str = "locked_only";

/// Collects machine-readable rejection counts and up to `MAX_TOP_REJECTED` (5) closest rejected
/// candidates without changing any scoring or selection decisions.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RejectionCollector {
    pub rejection_counts: BTreeMap<String, u32>,
    pub top_rejected: Vec<RejectedCandidateSummary>,
}

impl RejectionCollector {
    pub fn record(
        &mut self,
        source: impl Into<String>,
        label: impl Into<String>,
        score: Option<f32>,
        reason: impl Into<String>,
    ) {
        let reason = reason.into();
        *self.rejection_counts.entry(reason.clone()).or_default() += 1;
        let summary = RejectedCandidateSummary {
            source: source.into(),
            label: label.into(),
            score: score.filter(|s| s.is_finite()),
            reason,
        };
        self.push_top(summary);
    }

    fn push_top(&mut self, summary: RejectedCandidateSummary) {
        self.top_rejected.push(summary);
        self.top_rejected.sort_by(|a, b| {
            let key = |s: Option<f32>| match s {
                Some(v) if v.is_finite() => (1u8, v),
                _ => (0u8, f32::NEG_INFINITY),
            };
            let (ak, av) = key(a.score);
            let (bk, bv) = key(b.score);
            bk.cmp(&ak)
                .then_with(|| bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal))
        });
        self.top_rejected.truncate(MAX_TOP_REJECTED);
    }

    /// Merge all collected counts and top rejected candidates into `stats`.
    pub fn merge_into(&self, stats: &mut SearchStats) {
        for (reason, count) in &self.rejection_counts {
            *stats.rejection_counts.entry(reason.clone()).or_default() += *count;
        }
        for item in &self.top_rejected {
            stats.push_top_rejected(item.clone());
        }
    }

    /// Merge another collector into this one.
    pub fn extend(&mut self, other: RejectionCollector) {
        for (reason, count) in other.rejection_counts {
            *self.rejection_counts.entry(reason).or_default() += count;
        }
        for item in other.top_rejected {
            self.push_top(item);
        }
    }
}

/// Pure diagnostic classification for a file rejected by path-based scoring (`rank_match`).
/// Does not alter scoring or selection decisions.
pub fn classify_path_rejection(
    filename: &str,
    best: &Ranked,
    searched_artist: Option<&str>,
    expected_tracks: &[WantedTrack],
    file_length_s: Option<i32>,
) -> &'static str {
    if best.track.is_some() && best.result.track_score == 0.0 {
        return REASON_TITLE_MISMATCH;
    }

    if let Some(track_idx) = best.track {
        if let Some(wanted) = expected_tracks.get(track_idx) {
            let normalized = filename.replace('\\', "/");
            let stem = Path::new(&normalized)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(filename);
            if version_veto(stem, &wanted.title) {
                return REASON_VERSION_VETO;
            }
            if let (Some(wanted_s), Some(file_s)) = (
                wanted.duration_s,
                file_length_s.and_then(|l| u32::try_from(l).ok()),
            ) {
                if duration_score(wanted_s, file_s) == 0.0 {
                    return REASON_DURATION_DELTA;
                }
            }
        }
    }

    if searched_artist.is_some_and(|a| !a.trim().is_empty()) && best.result.artist_score == 0.0 {
        return REASON_ARTIST_GATE;
    }

    REASON_BELOW_THRESHOLD
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matching::path::rank_match;

    #[test]
    fn collector_keeps_at_most_five_highest_scoring_rejections() {
        let mut collector = RejectionCollector::default();
        for (idx, score) in [0.1, 0.59, 0.35, 0.52, 0.48, 0.57, 0.25]
            .into_iter()
            .enumerate()
        {
            collector.record(
                format!("peer_{idx}"),
                format!("file_{idx}.flac"),
                Some(score),
                REASON_BELOW_THRESHOLD,
            );
        }
        collector.record("peer_none", "locked", None, REASON_LOCKED_ONLY);

        assert_eq!(collector.rejection_counts[REASON_BELOW_THRESHOLD], 7);
        assert_eq!(collector.rejection_counts[REASON_LOCKED_ONLY], 1);
        assert_eq!(collector.top_rejected.len(), MAX_TOP_REJECTED);
        let kept: Vec<Option<f32>> = collector.top_rejected.iter().map(|r| r.score).collect();
        assert_eq!(
            kept,
            vec![Some(0.59), Some(0.57), Some(0.52), Some(0.48), Some(0.35)]
        );
    }

    #[test]
    fn classifies_title_version_duration_and_artist_rejections() {
        let wanted = [WantedTrack {
            title: "Wake Up".into(),
            duration_s: Some(240),
        }];

        let title_miss = rank_match(
            "Music/Kowloon/12 - Interview.flac",
            Some("Kowloon"),
            None,
            &wanted,
            Some(240),
        );
        assert_eq!(
            classify_path_rejection(
                "Music/Kowloon/12 - Interview.flac",
                &title_miss[0],
                Some("Kowloon"),
                &wanted,
                Some(240),
            ),
            REASON_TITLE_MISMATCH
        );

        let remix = rank_match(
            "Music/Other/01 - Wake Up (Club Remix).flac",
            Some("Kowloon"),
            None,
            &wanted,
            Some(240),
        );
        assert_eq!(
            classify_path_rejection(
                "Music/Other/01 - Wake Up (Club Remix).flac",
                &remix[0],
                Some("Kowloon"),
                &wanted,
                Some(240),
            ),
            REASON_VERSION_VETO
        );

        let wrong_len = rank_match(
            "Music/Kowloon/01 - Wake Up.flac",
            Some("Kowloon"),
            None,
            &wanted,
            Some(400),
        );
        assert_eq!(
            classify_path_rejection(
                "Music/Kowloon/01 - Wake Up.flac",
                &wrong_len[0],
                Some("Kowloon"),
                &wanted,
                Some(400),
            ),
            REASON_DURATION_DELTA
        );

        let wrong_artist = rank_match(
            "Music/Someone Else/01 - Wake Up Intro.flac",
            Some("Kowloon"),
            None,
            &wanted,
            Some(240),
        );
        assert_eq!(
            classify_path_rejection(
                "Music/Someone Else/01 - Wake Up Intro.flac",
                &wrong_artist[0],
                Some("Kowloon"),
                &wanted,
                Some(240),
            ),
            REASON_ARTIST_GATE
        );
    }
}
