// No longer need once_cell in Cargo.toml
// Add to Cargo.toml:
// regex = "1"
// serde = { version = "1.0", features = ["derive"] }
// serde_json = "1.0"

use regex::Regex;
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock; // Use the standard library's LazyLock

// Pre-compiled regex for performance, now using LazyLock
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

// --- Similarity Functions ---

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

// --- String Cleaning & Extraction ---

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

// --- Core Logic Structs ---

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

#[derive(Debug, Clone, Serialize)]
pub struct MatchResult {
    pub guessed_artist: String,
    pub guessed_album: String,
    pub matched_track: String,
    pub artist_score: f64,
    pub album_score: f64,
    pub track_score: f64,
    pub total_score: f64,
}

// --- Scoring Functions ---

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

fn score_track(stem: &CleanedText, expected_tracks: &[CleanedText]) -> (f64, CleanedText) {
    if expected_tracks.is_empty() {
        return (1.0, CleanedText::new(&extract_track_title(&stem.original)));
    }

    let track_title_from_stem = CleanedText::new(&extract_track_title(&stem.original));

    expected_tracks
        .iter()
        .map(|expected| {
            let score = (dice_sim(&track_title_from_stem, expected) * 0.6)
                + (containment_sim(&track_title_from_stem, expected) * 0.4);
            (score, expected.clone())
        })
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or((0.0, CleanedText::new("")))
}

// --- Public API ---

pub fn rank_match(
    filename: &str,
    searched_artist: Option<&str>,
    searched_album: Option<&str>,
    expected_tracks: &[&str],
) -> MatchResult {
    const ALBUM_WEIGHT: f64 = 0.4;
    const TRACK_WEIGHT: f64 = 0.4;
    const ARTIST_WEIGHT: f64 = 0.2;
    // If the album score is below this, we assume the path has no useful album info
    // and we don't penalize the score for it.
    const ALBUM_INFO_THRESHOLD: f64 = 0.25;

    // --- Component Scoring ---
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

    let (track_score, best_track_match) = if !expected_tracks.is_empty() {
        let expected_tracks_c: Vec<_> = expected_tracks
            .iter()
            .map(|t| CleanedText::new(t))
            .collect();
        score_track(&stem_c, &expected_tracks_c)
    } else {
        (
            0.0,
            CleanedText::new(&extract_track_title(&stem_c.original)),
        )
    };

    // --- Information-Aware Dynamic Weighting ---
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
    // Only factor in the album if it was searched for AND the path provided meaningful info.
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
