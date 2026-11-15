use regex::Regex;
use shared::slskd::MatchResult;
use std::{collections::HashSet, path::Path, sync::LazyLock};

static RE_NON_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^\w\s]").unwrap());
static RE_LEAD_TRACK_FIXED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(\d{1,3}|[A-D]\d{1,2})\s*[\.\-]\s*").unwrap());
static RE_TRAIL_BRACKET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*\[\s*[^\]]*\]\s*$").unwrap());
static RE_TRAIL_YEAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*[-\(\[]?\d{4}[-\)\]]?\s*$").unwrap());

// A struct to hold pre-processed text for efficient comparisons.
#[derive(Debug, Clone)]
struct CleanedText {
    original: String,
    words: HashSet<String>,
}

impl CleanedText {
    fn new(s: &str) -> Self {
        let original = s.to_string();
        let s = s.replace('_', " ");
        let cleaned = RE_NON_WORD.replace_all(&s, " ").to_string();
        let words = cleaned
            .to_lowercase()
            .split_whitespace()
            .filter(|w| !w.trim().is_empty())
            .map(|w| w.to_string())
            .collect();
        CleanedText { original, words }
    }

    pub fn words(&self) -> &HashSet<String> {
        &self.words
    }
}

fn jaccard_sim(a: &CleanedText, b: &CleanedText) -> f64 {
    let inter = a.words().intersection(b.words()).count();
    let uni = a.words().len() + b.words().len() - inter;
    if uni == 0 {
        0.0
    } else {
        inter as f64 / uni as f64
    }
}

fn containment_sim(candidate: &CleanedText, target: &CleanedText) -> f64 {
    if target.words().is_empty() {
        return 0.0;
    }
    let inter = candidate.words().intersection(target.words()).count();
    inter as f64 / target.words().len() as f64
}

fn dice_sim(a: &CleanedText, b: &CleanedText) -> f64 {
    let inter = a.words().intersection(b.words()).count();
    let total = a.words().len() + b.words().len();
    if total == 0 {
        1.0
    } else {
        (2.0 * inter as f64) / total as f64
    }
}

fn clean_name(name: &str) -> String {
    let name = name.replace('_', " ");
    let mut cleaned = RE_LEAD_TRACK_FIXED.replace(&name, "").to_string();
    cleaned = RE_TRAIL_BRACKET.replace(&cleaned, "").to_string();
    cleaned = RE_TRAIL_YEAR.replace(&cleaned, "").to_string();
    cleaned.trim().to_string()
}

fn extract_track_title(stem: &str) -> String {
    let stem_clean = clean_name(stem);
    if let Some(pos) = stem_clean.rfind(" - ") {
        stem_clean[pos + 3..].trim().to_string()
    } else {
        stem_clean
    }
}

#[derive(Debug)]
struct PathInfo {
    parent_folders: Vec<String>,
    stem: String,
}

impl PathInfo {
    fn from_path(path_str: &str) -> Self {
        let normalized_path_str = path_str.replace('\\', "/");
        let path = Path::new(&normalized_path_str);

        let parent_folders = path
            .ancestors()
            .skip(1)
            .filter_map(|p| p.file_name())
            .filter_map(|s| s.to_str())
            .map(|s| s.to_string())
            .collect::<Vec<_>>();

        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        let mut reversed_folders = parent_folders;
        reversed_folders.reverse();

        Self {
            parent_folders: reversed_folders,
            stem,
        }
    }
}

fn score_album(folders: &[CleanedText], target_album: &CleanedText) -> (f64, CleanedText) {
    folders
        .iter()
        .map(|folder| {
            let score =
                (jaccard_sim(folder, target_album) + containment_sim(folder, target_album)) / 2.0;
            (score, folder.clone())
        })
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or((0.0, CleanedText::new("")))
}

fn score_artist(
    folders: &[CleanedText],
    stem: &CleanedText,
    target_artist: &CleanedText,
) -> (f64, CleanedText) {
    let folder_candidate = folders
        .iter()
        .map(|folder| (containment_sim(folder, target_artist), folder.clone()))
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or((0.0, CleanedText::new("")));

    let stem_artist_part = if let Some(pos) = stem.original.rfind(" - ") {
        clean_name(&stem.original[..pos])
    } else {
        clean_name(&stem.original)
    };
    let stem_candidate_c = CleanedText::new(&stem_artist_part);
    let stem_score = containment_sim(&stem_candidate_c, target_artist);

    if stem_score > folder_candidate.0 {
        return (stem_score, stem_candidate_c);
    }
    if folder_candidate.0 > stem_score {
        return folder_candidate;
    }

    if stem_score > 0.9 && stem_candidate_c.original.len() > folder_candidate.1.original.len() {
        return (stem_score, stem_candidate_c);
    }
    folder_candidate
}

/// The best score among `expected_tracks` (not empty) and the index of that track.
fn score_track(stem: &CleanedText, expected_tracks: &[CleanedText]) -> (f64, usize) {
    let track_title_from_stem = CleanedText::new(&extract_track_title(&stem.original));

    expected_tracks
        .iter()
        .enumerate()
        .map(|(i, expected)| {
            let score = (dice_sim(&track_title_from_stem, expected) * 0.6)
                + (containment_sim(&track_title_from_stem, expected) * 0.4);
            (score, i)
        })
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or((0.0, 0))
}

/// A track the search is for, with its length when the metadata provider knows it.
#[derive(Debug, Clone)]
pub struct WantedTrack {
    pub title: String,
    pub duration_s: Option<u32>,
}

pub const DURATION_WEIGHT: f64 = 0.3;

/// Full credit up to this many seconds of difference, none from `DURATION_NONE_S` on.
const DURATION_FULL_S: u32 = 3;
const DURATION_NONE_S: u32 = 30;

fn duration_score(wanted_s: u32, file_s: u32) -> f64 {
    let diff = wanted_s.abs_diff(file_s);
    if diff <= DURATION_FULL_S {
        1.0
    } else if diff >= DURATION_NONE_S {
        0.0
    } else {
        1.0 - f64::from(diff - DURATION_FULL_S) / f64::from(DURATION_NONE_S - DURATION_FULL_S)
    }
}

pub fn rank_match(
    filename: &str,
    searched_artist: Option<&str>,
    searched_album: Option<&str>,
    expected_tracks: &[WantedTrack],
    file_length_s: Option<i32>,
) -> MatchResult {
    const ALBUM_WEIGHT: f64 = 0.4;
    const TRACK_WEIGHT: f64 = 0.4;
    const ARTIST_WEIGHT: f64 = 0.2;
    // If the album score is below this, we assume the path has no useful album info
    // and we don't penalize the score for it.
    const ALBUM_INFO_THRESHOLD: f64 = 0.25;

    let path_info = PathInfo::from_path(filename);
    let path_folders_c: Vec<_> = path_info
        .parent_folders
        .iter()
        .map(|f| CleanedText::new(&clean_name(f)))
        .collect();
    let stem_c = CleanedText::new(&path_info.stem);

    let (artist_score, best_artist_guess) = if let Some(artist_str) = searched_artist {
        let searched_artist_c = CleanedText::new(artist_str);
        score_artist(&path_folders_c, &stem_c, &searched_artist_c)
    } else {
        (0.0, CleanedText::new(""))
    };

    let (album_score, best_album_folder) = if let Some(album_str) = searched_album {
        let searched_album_c = CleanedText::new(album_str);
        score_album(&path_folders_c, &searched_album_c)
    } else {
        (0.0, CleanedText::new(""))
    };

    let (track_score, best_track_match, matched_duration_s) = if !expected_tracks.is_empty() {
        let expected_tracks_c: Vec<_> = expected_tracks
            .iter()
            .map(|t| CleanedText::new(&t.title))
            .collect();
        let (score, i) = score_track(&stem_c, &expected_tracks_c);
        (
            score,
            expected_tracks_c[i].clone(),
            expected_tracks[i].duration_s,
        )
    } else {
        (
            0.0,
            CleanedText::new(&extract_track_title(&stem_c.original)),
            None,
        )
    };

    let mut weighted_sum = 0.0;
    let mut total_weight = 0.0;

    if searched_artist.is_some() {
        weighted_sum += artist_score * ARTIST_WEIGHT;
        total_weight += ARTIST_WEIGHT;
    }

    if !expected_tracks.is_empty() {
        weighted_sum += track_score * TRACK_WEIGHT;
        total_weight += TRACK_WEIGHT;
    }

    if let (Some(wanted_s), Some(file_s)) = (
        matched_duration_s,
        file_length_s.and_then(|l| u32::try_from(l).ok()),
    ) {
        weighted_sum += duration_score(wanted_s, file_s) * DURATION_WEIGHT;
        total_weight += DURATION_WEIGHT;
    }

    if searched_album.is_some() {
        weighted_sum += album_score * ALBUM_WEIGHT;
        if album_score > ALBUM_INFO_THRESHOLD {
            total_weight += ALBUM_WEIGHT;
        }
    }

    let total_score = if total_weight > 0.0 {
        weighted_sum / total_weight
    } else {
        0.0
    };

    MatchResult {
        guessed_artist: best_artist_guess.original,
        guessed_album: best_album_folder.original,
        matched_track: best_track_match.original,
        artist_score,
        album_score,
        track_score,
        total_score,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "Marrow Choir\\Winter Songs\\03 - O Holy Night.flac";

    fn night(duration_s: Option<u32>) -> Vec<WantedTrack> {
        vec![WantedTrack {
            title: "O Holy Night".into(),
            duration_s,
        }]
    }

    #[test]
    fn a_file_of_the_wanted_length_keeps_its_score() {
        let score = |wanted, length| {
            rank_match(FILE, Some("Marrow Choir"), None, &night(wanted), length).total_score
        };
        assert!((score(Some(263), Some(263)) - 1.0).abs() < 1e-9);
        assert!((score(Some(263), Some(266)) - 1.0).abs() < 1e-9);
        assert!((score(None, Some(323)) - 1.0).abs() < 1e-9);
        assert!((score(Some(263), None) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn another_length_lowers_the_match_score() {
        let score = |length| {
            rank_match(
                FILE,
                Some("Marrow Choir"),
                None,
                &night(Some(263)),
                Some(length),
            )
            .total_score
        };
        assert!((score(273) - (0.6 + 0.3 * 20.0 / 27.0) / 0.9).abs() < 1e-9);
        assert!((score(323) - 2.0 / 3.0).abs() < 1e-9);
        assert!((score(203) - 2.0 / 3.0).abs() < 1e-9);
    }

    /// The path names the searched album, so the album term (0.4) counts too.
    #[test]
    fn with_the_album_in_the_path_another_length_scores_ten_thirteenths() {
        let score = |length| {
            rank_match(
                FILE,
                Some("Marrow Choir"),
                Some("Winter Songs"),
                &night(Some(263)),
                Some(length),
            )
            .total_score
        };
        assert!((score(263) - 1.0).abs() < 1e-9);
        assert!((score(323) - 10.0 / 13.0).abs() < 1e-9);
    }
}
