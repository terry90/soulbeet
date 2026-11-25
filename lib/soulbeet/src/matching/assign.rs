//! Which file of a group stands for which wanted track.

use super::path::WantedTrack;
use std::collections::HashSet;

/// For each of `wanted`, the index of the file it takes in `files` and that file's score as it,
/// or None. Every pair of a file and a track `score` scores (None: the file is not that track)
/// is taken best score first, the better `quality` first between equal scores, the earlier file
/// then the earlier track between equal pairs; each track takes the best file no other track
/// took, so a title a release repeats gets a file per track and no file stands for two. The
/// files must be distinct.
pub fn assign_tracks<F>(
    wanted: &[WantedTrack],
    files: &[F],
    score: impl Fn(&F, usize, &WantedTrack) -> Option<f64>,
    quality: impl Fn(&F) -> f64,
) -> Vec<Option<(usize, f64)>> {
    let mut pairs: Vec<(usize, usize, f64)> = Vec::new();
    for (f, file) in files.iter().enumerate() {
        for (t, track) in wanted.iter().enumerate() {
            if let Some(s) = score(file, t, track) {
                pairs.push((f, t, s));
            }
        }
    }
    // Stable, so equal pairs keep the files' and the tracks' order.
    pairs.sort_by(|&(f1, _, s1), &(f2, _, s2)| {
        s2.partial_cmp(&s1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                quality(&files[f2])
                    .partial_cmp(&quality(&files[f1]))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
    let mut taken: Vec<Option<(usize, f64)>> = vec![None; wanted.len()];
    let mut used: HashSet<usize> = HashSet::new();
    for (f, t, s) in pairs {
        if taken[t].is_none() && used.insert(f) {
            taken[t] = Some((f, s));
        }
    }
    taken
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wanted(titles: &[&str]) -> Vec<WantedTrack> {
        titles
            .iter()
            .map(|t| WantedTrack {
                title: t.to_string(),
                duration_s: None,
            })
            .collect()
    }

    /// A file: its scores as each wanted track, and its quality.
    struct File(Vec<Option<f64>>, f64);

    fn assigned(wanted: &[WantedTrack], files: &[File]) -> Vec<Option<(usize, f64)>> {
        assign_tracks(wanted, files, |f, t, _| f.0[t], |f| f.1)
    }

    #[test]
    fn each_track_takes_the_best_file_no_other_track_took() {
        let tracks = wanted(&["O Holy Night", "Silent Night", "O Holy Night"]);
        let files = [
            File(vec![Some(0.9), None, Some(0.8)], 0.5),
            File(vec![None, Some(0.7), None], 0.5),
            File(vec![Some(0.8), None, Some(0.9)], 0.5),
        ];
        assert_eq!(
            assigned(&tracks, &files),
            vec![Some((0, 0.9)), Some((1, 0.7)), Some((2, 0.9))]
        );
    }

    #[test]
    fn between_equal_scores_the_better_quality_then_the_earlier_file_wins() {
        let tracks = wanted(&["Wake Up"]);
        let files = [
            File(vec![Some(0.9)], 0.55),
            File(vec![Some(0.9)], 1.0),
            File(vec![Some(0.9)], 1.0),
        ];
        assert_eq!(assigned(&tracks, &files), vec![Some((1, 0.9))]);
        let same = [File(vec![Some(0.9)], 1.0), File(vec![Some(0.9)], 1.0)];
        assert_eq!(assigned(&tracks, &same), vec![Some((0, 0.9))]);
    }

    #[test]
    fn a_track_no_file_scores_takes_none_and_a_file_stands_for_one_track() {
        let tracks = wanted(&["Wake Up", "Come Over"]);
        let files = [File(vec![Some(0.9), Some(0.95)], 1.0)];
        assert_eq!(assigned(&tracks, &files), vec![None, Some((0, 0.95))]);
        assert_eq!(assigned(&tracks, &[]), vec![None, None]);
    }
}
