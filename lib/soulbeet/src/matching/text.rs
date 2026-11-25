//! Word-set similarity of names and titles.

use regex::Regex;
use std::{collections::HashSet, sync::LazyLock};

static RE_NON_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^\w\s]").unwrap());
static RE_LEAD_TRACK_FIXED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(\d{1,3}|[A-D]\d{1,2})\s*[\.\-]\s*").unwrap());
static RE_TRAIL_BRACKET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*\[\s*[^\]]*\]\s*$").unwrap());
static RE_TRAIL_YEAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*[-\(\[]?\d{4}[-\)\]]?\s*$").unwrap());

/// A text and its lowercase words, for comparing it many times.
#[derive(Debug, Clone)]
pub struct CleanedText {
    pub original: String,
    words: HashSet<String>,
}

impl CleanedText {
    pub fn new(s: &str) -> Self {
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

pub fn jaccard_sim(a: &CleanedText, b: &CleanedText) -> f64 {
    let inter = a.words().intersection(b.words()).count();
    let uni = a.words().len() + b.words().len() - inter;
    if uni == 0 {
        0.0
    } else {
        inter as f64 / uni as f64
    }
}

/// The share of `target`'s words that `candidate` has.
pub fn containment_sim(candidate: &CleanedText, target: &CleanedText) -> f64 {
    if target.words().is_empty() {
        return 0.0;
    }
    let inter = candidate.words().intersection(target.words()).count();
    inter as f64 / target.words().len() as f64
}

pub fn dice_sim(a: &CleanedText, b: &CleanedText) -> f64 {
    let inter = a.words().intersection(b.words()).count();
    let total = a.words().len() + b.words().len();
    if total == 0 {
        1.0
    } else {
        (2.0 * inter as f64) / total as f64
    }
}

/// A file or folder name without its leading track number, trailing bracket and trailing year.
pub fn clean_name(name: &str) -> String {
    let name = name.replace('_', " ");
    let mut cleaned = RE_LEAD_TRACK_FIXED.replace(&name, "").to_string();
    cleaned = RE_TRAIL_BRACKET.replace(&cleaned, "").to_string();
    cleaned = RE_TRAIL_YEAR.replace(&cleaned, "").to_string();
    cleaned.trim().to_string()
}

/// The title part of a file stem: after its last ` - `, cleaned.
pub fn extract_track_title(stem: &str) -> String {
    let stem_clean = clean_name(stem);
    if let Some(pos) = stem_clean.rfind(" - ") {
        stem_clean[pos + 3..].trim().to_string()
    } else {
        stem_clean
    }
}

/// How well the folders or the stem name `target_artist`, and the text that names it best.
pub fn score_artist(
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

/// How well the title in `stem` matches each of `expected_tracks`.
pub fn title_scores(stem: &CleanedText, expected_tracks: &[CleanedText]) -> Vec<f64> {
    let track_title_from_stem = CleanedText::new(&extract_track_title(&stem.original));

    expected_tracks
        .iter()
        .map(|expected| {
            (dice_sim(&track_title_from_stem, expected) * 0.6)
                + (containment_sim(&track_title_from_stem, expected) * 0.4)
        })
        .collect()
}
