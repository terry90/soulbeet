use super::utils::{self, WantedTrack};
use crate::audio::is_audio_path;
use crate::slskd::models::SearchResponse;
use itertools::Itertools;
use shared::slskd::{declared_extension, AlbumResult, MatchResult, SearchResult, TrackResult};
use std::collections::HashMap;
use std::path::Path;

/// The albums of a search's responses, and what was left out or flagged.
pub struct Processed {
    pub albums: Vec<AlbumResult>,
    /// Files with an extension that is not audio.
    pub not_audio: u32,
    /// Audio files scored below the floor, kept or not.
    pub below_floor: u32,
}

/// Processing keeps a file as audio when its name has no extension or an audio one.
fn kept_as_audio(filename: &str) -> bool {
    let path = Path::new(filename);
    path.extension().is_none() || is_audio_path(path)
}

/// The remote folder of a file a peer names with `\` or `/` between its folders.
fn remote_folder(filename: &str) -> &str {
    filename
        .rsplit_once(['\\', '/'])
        .map_or("", |(folder, _)| folder)
}

/// The albums found in `responses`. A file scored under MIN_SCORE_THRESHOLD
/// is dropped, or with `keep_below_floor` kept and flagged `below_floor`.
pub fn process_search_responses(
    responses: &[SearchResponse],
    searched_artist: &str,
    searched_album: Option<&str>,
    expected_tracks: &[WantedTrack],
    keep_below_floor: bool,
) -> Processed {
    const MIN_SCORE_THRESHOLD: f64 = 0.6;
    let mut not_audio = 0;
    let mut below_floor = 0;
    let mut scored_files: Vec<(MatchResult, SearchResult, bool)> = Vec::new();
    for resp in responses {
        let mut folder_audio_files: HashMap<&str, u32> = HashMap::new();
        for file in resp.files.iter().filter(|f| kept_as_audio(&f.filename)) {
            *folder_audio_files
                .entry(remote_folder(&file.filename))
                .or_default() += 1;
        }
        for file in &resp.files {
            if !kept_as_audio(&file.filename) {
                not_audio += 1;
                continue;
            }

            let rank_result = utils::rank_match(
                &file.filename,
                Some(searched_artist),
                searched_album,
                expected_tracks,
                file.length,
            );

            let flagged = rank_result.total_score < MIN_SCORE_THRESHOLD;
            if flagged {
                below_floor += 1;
                if !keep_below_floor {
                    continue;
                }
            }

            let search_result = SearchResult {
                username: resp.username.clone(),
                filename: file.filename.clone(),
                size: file.size,
                bitrate: file.bit_rate,
                duration: file.length,
                sample_rate: file.sample_rate,
                bit_depth: file.bit_depth,
                has_free_upload_slot: resp.has_free_upload_slot,
                upload_speed: resp.upload_speed,
                queue_length: resp.queue_length,
                variable_bit_rate: file.is_variable_bit_rate,
                declared_extension: file.extension.as_deref().and_then(declared_extension),
                folder_audio_files: folder_audio_files[remote_folder(&file.filename)],
            };
            scored_files.push((rank_result, search_result, flagged));
        }
    }

    Processed {
        albums: find_best_albums(&scored_files, expected_tracks),
        not_audio,
        below_floor,
    }
}

fn find_best_albums(
    scored_files: &[(MatchResult, SearchResult, bool)],
    expected_tracks: &[WantedTrack],
) -> Vec<AlbumResult> {
    if expected_tracks.is_empty() {
        return vec![];
    }

    let album_groups = scored_files.iter().into_group_map_by(|(rank, search, _)| {
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

            for expected in expected_tracks {
                let expected_track_title = expected.title.as_str();
                if let Some(best_file_for_track) = files_in_group
                    .iter()
                    // Find all files that matched this specific track
                    .filter(|(rank, _, _)| rank.matched_track == expected_track_title)
                    // Find the best one among them
                    .max_by(|(r1, s1, _), (r2, s2, _)| {
                        r1.total_score
                            .partial_cmp(&r2.total_score)
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then_with(|| {
                                s1.quality_score().partial_cmp(&s2.quality_score()).unwrap()
                            })
                    })
                {
                    best_files_for_album.insert(expected_track_title, best_file_for_track);
                }
            }

            let final_tracks: Vec<_> = expected_tracks
                .iter()
                .filter_map(|t| best_files_for_album.get(t.title.as_str()))
                .map(|(mr, sr, flagged)| TrackResult {
                    below_floor: *flagged,
                    ..TrackResult::new(sr.clone(), mr.clone())
                })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slskd::models::SearchResponseFile;

    #[test]
    fn a_folders_audio_files_are_counted_per_folder_whichever_separator_names_it() {
        assert_eq!(
            remote_folder("@@kqzbm\\Music\\Come Over\\07.flac"),
            "@@kqzbm\\Music\\Come Over"
        );
        assert_eq!(remote_folder("music/Come Over/07.flac"), "music/Come Over");
        assert_eq!(remote_folder("07.flac"), "");
        let file = |name: &str| SearchResponseFile {
            filename: name.into(),
            size: 1,
            bit_rate: None,
            length: None,
            sample_rate: None,
            bit_depth: None,
            extension: None,
            is_variable_bit_rate: None,
        };
        let response = SearchResponse {
            username: "carol_shares".into(),
            files: vec![
                file("music/Come Over/07 - Wake Up.flac"),
                file("music/Come Over/08 - Come Over.flac"),
                file("music/Come Over/cover.jpg"),
                file("music/Singles/01 - Wake Up.flac"),
            ],
            has_free_upload_slot: true,
            upload_speed: 1,
            queue_length: 0,
            locked_file_count: 0,
        };
        let wanted = [WantedTrack {
            title: "Wake Up".into(),
            duration_s: None,
        }];
        let processed = process_search_responses(&[response], "Kowloon", None, &wanted, true);
        let mut counts: Vec<(String, u32)> = (processed.albums.iter())
            .flat_map(|album| &album.tracks)
            .map(|track| (track.base.filename.clone(), track.base.folder_audio_files))
            .collect();
        counts.sort();
        assert_eq!(
            counts,
            // Come Over's two audio files, its cover not counted; the single's folder its one.
            vec![
                ("music/Come Over/07 - Wake Up.flac".to_string(), 2),
                ("music/Singles/01 - Wake Up.flac".to_string(), 1),
            ]
        );
    }
}
