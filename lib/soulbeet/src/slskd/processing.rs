use super::utils;
use crate::slskd::models::SearchResponse;
use itertools::Itertools;
use shared::slskd::{AlbumResult, MatchResult, SearchResult, TrackResult};
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub fn process_search_responses(
    responses: &[SearchResponse],
    searched_artist: &str,
    searched_album: &str,
    expected_tracks: &[&str],
) -> Vec<AlbumResult> {
    const MIN_SCORE_THRESHOLD: f64 = 0.6;
    let audio_extensions: HashSet<&str> = ["flac", "wav", "m4a", "ogg", "aac", "wma", "mp3"]
        .iter()
        .copied()
        .collect();

    let scored_files: Vec<(MatchResult, SearchResult)> = responses
        .iter()
        .flat_map(|resp| {
            resp.files.iter().filter_map(|file| {
                let path = Path::new(&file.filename);
                let ext = path
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_lowercase());

                if let Some(ext) = ext {
                    if !audio_extensions.contains(ext.as_str()) {
                        return None;
                    }
                }

                let rank_result = utils::rank_match(
                    &file.filename,
                    Some(searched_artist),
                    Some(searched_album),
                    expected_tracks,
                );

                if rank_result.total_score < MIN_SCORE_THRESHOLD {
                    return None;
                }

                let search_result = SearchResult {
                    username: resp.username.clone(),
                    filename: file.filename.clone(),
                    size: file.size,
                    bitrate: file.bit_rate,
                    duration: file.length,
                    has_free_upload_slot: resp.has_free_upload_slot,
                    upload_speed: resp.upload_speed,
                    queue_length: resp.queue_length,
                };
                Some((rank_result, search_result))
            })
        })
        .collect();

    find_best_albums(&scored_files, expected_tracks)
}

fn find_best_albums(
    scored_files: &[(MatchResult, SearchResult)],
    expected_tracks: &[&str],
) -> Vec<AlbumResult> {
    if expected_tracks.is_empty() {
        return vec![];
    }

    let album_groups = scored_files.iter().into_group_map_by(|(rank, search)| {
        (
            search.username.clone(),
            rank.guessed_artist.clone(),
            rank.guessed_album.clone(),
        )
    });

    album_groups
        .into_iter()
        .filter_map(|((username, artist, album_title), files_in_group)| {
            // Specific search: find the single best file for each expected track.
            let mut best_files_for_album = HashMap::new();

            for expected_track_title in expected_tracks {
                if let Some(best_file_for_track) = files_in_group
                    .iter()
                    // Find all files that matched this specific track
                    .filter(|(rank, _)| &rank.matched_track == expected_track_title)
                    // Find the best one among them
                    .max_by(|(r1, s1), (r2, s2)| {
                        r1.total_score
                            .partial_cmp(&r2.total_score)
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then_with(|| {
                                s1.quality_score().partial_cmp(&s2.quality_score()).unwrap()
                            })
                    })
                {
                    best_files_for_album.insert(*expected_track_title, best_file_for_track);
                }
            }

            let final_tracks: Vec<_> = expected_tracks
                .iter()
                .filter_map(|t| best_files_for_album.get(*t))
                .map(|(mr, sr)| TrackResult::new(sr.clone(), mr.clone()))
                .collect();

            if final_tracks.is_empty() {
                return None;
            }

            let completeness = if !expected_tracks.is_empty() {
                final_tracks.len() as f64 / expected_tracks.len() as f64
            } else {
                1.0
            };

            let total_size: i64 = final_tracks.iter().map(|t| t.base.size).sum();
            let dominant_quality = final_tracks
                .iter()
                .map(|t| t.base.quality())
                .counts()
                .into_iter()
                .max_by_key(|&(_, count)| count)
                .map(|(val, _)| val)
                .unwrap_or_default();

            let first_track = final_tracks[0].base.clone();
            let album_path = first_track.filename.clone();

            let avg_score: f64 =
                final_tracks.iter().map(|t| t.match_score).sum::<f64>() / final_tracks.len() as f64;
            let avg_format_score = final_tracks
                .iter()
                .map(|t| t.base.quality_score())
                .sum::<f64>()
                / final_tracks.len() as f64;

            let album_quality_score =
                (avg_score * 0.3) + (completeness * 0.3) + (avg_format_score * 0.4);

            Some(AlbumResult {
                username,
                album_path,
                album_title,
                artist: Some(artist),
                track_count: final_tracks.len(),
                total_size,
                tracks: final_tracks,
                dominant_quality,
                has_free_upload_slot: first_track.has_free_upload_slot,
                upload_speed: first_track.upload_speed,
                queue_length: first_track.queue_length,
                score: album_quality_score,
            })
        })
        .collect()
}
