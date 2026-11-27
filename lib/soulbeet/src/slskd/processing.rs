use crate::audio::is_audio_path;
use crate::matching::assign::assign_tracks;
use crate::matching::diagnostics::{classify_path_rejection, RejectionCollector, REASON_LOCKED_ONLY};
use crate::matching::path::{rank_match, release_folder, remote_folder, Ranked, WantedTrack};
use crate::matching::score::{group_score, MIN_SCORE_THRESHOLD};
use crate::slskd::models::SearchResponse;
use itertools::Itertools;
use shared::download::RejectedCandidateSummary;
use shared::slskd::{declared_extension, AlbumResult, MatchResult, SearchResult, TrackResult};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

/// The albums of a search's responses, and what was left out or flagged.
pub struct Processed {
    pub albums: Vec<AlbumResult>,
    /// Files with an extension that is not audio.
    pub not_audio: u32,
    /// Audio files scored below the floor, kept or not.
    pub below_floor: u32,
    /// Machine-readable rejection counts across pre-enqueue filtering.
    pub rejection_counts: BTreeMap<String, u32>,
    /// Up to 5 closest rejected candidates with the reason each was dropped.
    pub top_rejected: Vec<RejectedCandidateSummary>,
}

/// Processing keeps a file as audio when its name has no extension or an audio one.
fn kept_as_audio(filename: &str) -> bool {
    let path = Path::new(filename);
    path.extension().is_none() || is_audio_path(path)
}

/// A file as one of the expected tracks it may be.
struct Candidate {
    /// The track's index in the expected tracks.
    track: usize,
    result: MatchResult,
    below_floor: bool,
}

/// The albums found in `responses`. A file scored under MIN_SCORE_THRESHOLD,
/// or matching no word of any wanted title, is dropped, or with
/// `keep_below_floor` kept and flagged `below_floor`.
pub fn process_search_responses(
    responses: &[SearchResponse],
    searched_artist: &str,
    searched_album: Option<&str>,
    expected_tracks: &[WantedTrack],
    keep_below_floor: bool,
) -> Processed {
    let mut not_audio = 0;
    let mut below_floor = 0;
    let mut rejections = RejectionCollector::default();
    let mut scored_files: Vec<(Vec<Candidate>, SearchResult)> = Vec::new();
    // A file that shares no word with any wanted title is none of them,
    // whatever its folder and length say. Every ranking of a file has the
    // same title score.
    let under = |r: &Ranked| {
        r.result.total_score < MIN_SCORE_THRESHOLD
            || (r.track.is_some() && r.result.track_score == 0.0)
    };
    for resp in responses {
        if resp.files.is_empty() && resp.locked_file_count > 0 {
            rejections.record(
                resp.username.clone(),
                format!("{} locked files", resp.locked_file_count),
                None,
                REASON_LOCKED_ONLY,
            );
        }
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

            let ranked = rank_match(
                &file.filename,
                Some(searched_artist),
                searched_album,
                expected_tracks,
                file.length,
            );

            // Best first, so the first is the file's own score.
            let flagged = under(&ranked[0]);
            if flagged {
                below_floor += 1;
                let reason = classify_path_rejection(
                    &file.filename,
                    &ranked[0],
                    Some(searched_artist),
                    expected_tracks,
                    file.length,
                );
                rejections.record(
                    resp.username.clone(),
                    file.filename.clone(),
                    Some(ranked[0].result.total_score as f32),
                    reason,
                );
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
            // A track the file scores below the floor as is one it may not be,
            // unless the search keeps such files.
            let candidates = ranked
                .into_iter()
                .filter_map(|r| {
                    Some(Candidate {
                        below_floor: under(&r),
                        track: r.track?,
                        result: r.result,
                    })
                })
                .filter(|c| keep_below_floor || !c.below_floor)
                .collect();
            scored_files.push((candidates, search_result));
        }
    }

    Processed {
        albums: find_best_albums(&scored_files, expected_tracks),
        not_audio,
        below_floor,
        rejection_counts: rejections.rejection_counts,
        top_rejected: rejections.top_rejected,
    }
}

/// The candidate a file is as the expected track `track`.
fn candidate(file: &(Vec<Candidate>, SearchResult), track: usize) -> Option<&Candidate> {
    file.0.iter().find(|c| c.track == track)
}

fn find_best_albums(
    scored_files: &[(Vec<Candidate>, SearchResult)],
    expected_tracks: &[WantedTrack],
) -> Vec<AlbumResult> {
    if expected_tracks.is_empty() {
        return vec![];
    }

    // One album per release folder of a peer, so the discs of a release are
    // one album whatever names its folders.
    let album_groups = scored_files.iter().into_group_map_by(|(_, search)| {
        (
            search.username.clone(),
            release_folder(&search.filename).to_string(),
        )
    });

    album_groups
        .into_iter()
        .filter_map(|((username, _), files_in_group)| {
            // A file a peer lists twice is one file.
            let mut seen: HashSet<&str> = HashSet::new();
            let files: Vec<&(Vec<Candidate>, SearchResult)> = files_in_group
                .into_iter()
                .filter(|(_, search)| seen.insert(search.filename.as_str()))
                .collect();
            let taken: Vec<Option<(&Candidate, &SearchResult)>> = assign_tracks(
                expected_tracks,
                &files,
                |file, track, _| candidate(file, track).map(|c| c.result.total_score),
                |(_, search)| search.quality_score(),
            )
            .into_iter()
            .enumerate()
            .map(|(track, took)| {
                took.and_then(|(f, _)| Some((candidate(files[f], track)?, &files[f].1)))
            })
            .collect();

            let final_tracks: Vec<_> = taken
                .into_iter()
                .flatten()
                .map(|(candidate, search)| TrackResult {
                    below_floor: candidate.below_floor,
                    ..TrackResult::new(search.clone(), candidate.result.clone())
                })
                .collect();

            if final_tracks.is_empty() {
                return None;
            }

            let completeness = final_tracks.len() as f64 / expected_tracks.len() as f64;

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

            let album_quality_score = group_score(avg_score, completeness, avg_format_score);

            // Every file of a release folder guesses the same album.
            let album_title = final_tracks[0].album.clone();
            let artist = final_tracks[0].artist.clone();

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

    #[test]
    fn a_file_matching_no_wanted_title_fills_no_track() {
        let file = |name: &str, length: i32| SearchResponseFile {
            filename: format!("@@kqzbm\\Music\\Marrow Choir\\Winter Songs\\{name}"),
            size: 1,
            bit_rate: None,
            length: Some(length),
            sample_rate: None,
            bit_depth: None,
            extension: None,
            is_variable_bit_rate: None,
        };
        // The peer lacks "Away in a Manger" and shares an interview of its length.
        let response = SearchResponse {
            username: "carol_shares".into(),
            files: vec![
                file("03 - O Holy Night.flac", 263),
                file("04 - Silent Night.flac", 149),
                file("12 - Interview.flac", 181),
            ],
            has_free_upload_slot: true,
            upload_speed: 1,
            queue_length: 0,
            locked_file_count: 0,
        };
        let tracks = wanted(&[
            ("O Holy Night", 263),
            ("Silent Night", 149),
            ("Away in a Manger", 181),
        ]);
        let processed = process_search_responses(
            &[response],
            "Marrow Choir",
            Some("Winter Songs"),
            &tracks,
            false,
        );
        assert_eq!(processed.below_floor, 1);
        assert_eq!(processed.albums.len(), 1);
        assert_eq!(
            album_files(&processed.albums[0]),
            vec![
                "@@kqzbm\\Music\\Marrow Choir\\Winter Songs\\03 - O Holy Night.flac",
                "@@kqzbm\\Music\\Marrow Choir\\Winter Songs\\04 - Silent Night.flac",
            ]
        );
    }

    /// slskd's answer to `GET /api/v0/searches/{id}/responses` (`Search/Types/Response.cs:38-48`)
    /// from a peer sharing a two-disc release, one folder per disc.
    const TWO_DISCS: &str = include_str!("fixtures/search_two_discs.json");
    /// The same from a peer whose second disc repeats a title of the first.
    const REPEATED_TITLE: &str = include_str!("fixtures/search_two_discs_repeated_title.json");

    fn wanted(tracks: &[(&str, u32)]) -> Vec<WantedTrack> {
        tracks
            .iter()
            .map(|&(title, duration_s)| WantedTrack {
                title: title.into(),
                duration_s: Some(duration_s),
            })
            .collect()
    }

    fn album_files(album: &AlbumResult) -> Vec<&str> {
        album
            .tracks
            .iter()
            .map(|t| t.base.filename.as_str())
            .collect()
    }

    #[test]
    fn a_two_disc_share_is_one_album_whatever_the_album_is_called() {
        assert_eq!(
            release_folder("@@lq4ve\\Album\\CD1\\01.flac"),
            "@@lq4ve\\Album"
        );
        assert_eq!(release_folder("music/Album/Disc 2/01.flac"), "music/Album");
        assert_eq!(release_folder("music/Album/01.flac"), "music/Album");
        assert_eq!(release_folder("CD1/01.flac"), "");

        let responses: Vec<SearchResponse> = serde_json::from_str(TWO_DISCS).unwrap();
        // Bigger Than All of Us, MusicBrainz release cf554d3c-52be-4609-8f87-ea7fb47a82fe:
        // three tracks of its first disc and two of its second.
        let tracks = wanted(&[
            ("Stepping In", 248),
            ("Start a Fire", 229),
            ("Carry Me Home", 256),
            ("Start a Fire (extended mix)", 333),
            ("Carry Me Home (extended mix)", 378),
        ]);
        let release =
            "@@lq4ve\\Music\\Above & Beyond\\Above & Beyond - Bigger Than All of Us (2026) [FLAC]";
        let in_release = |file: &str| format!("{release}\\{file}");
        let both_discs = [
            in_release("CD1\\01 - Stepping In.flac"),
            in_release("CD1\\02 - Start a Fire.flac"),
            in_release("CD1\\03 - Carry Me Home.flac"),
            in_release("CD2\\01 - Start a Fire (Extended Mix).flac"),
            in_release("CD2\\02 - Carry Me Home (Extended Mix).flac"),
        ];
        // The title the share names, and one no folder of it shares a word
        // with, as a translated or romanised title does.
        for title in ["Bigger Than All of Us", "Größer als wir alle"] {
            let processed =
                process_search_responses(&responses, "Above & Beyond", Some(title), &tracks, false);
            assert_eq!(processed.albums.len(), 1, "{title}");
            let album = &processed.albums[0];
            assert_eq!(album_files(album), both_discs, "{title}");
            assert_eq!(album.album_title, "Above & Beyond - Bigger Than All of Us");
        }
    }

    #[test]
    fn a_title_on_both_discs_takes_a_file_from_each() {
        let responses: Vec<SearchResponse> = serde_json::from_str(REPEATED_TITLE).unwrap();
        // The second "O Holy Night" is the live one of the second disc.
        let deluxe = wanted(&[
            ("O Holy Night", 263),
            ("Silent Night", 149),
            ("O Holy Night", 301),
        ]);
        let release = "@@kqzbm\\Music\\Marrow Choir\\Winter Songs (Deluxe Edition)";
        let processed = process_search_responses(
            &responses,
            "Marrow Choir",
            Some("Winter Songs"),
            &deluxe,
            false,
        );
        assert_eq!(processed.albums.len(), 1);
        assert_eq!(
            album_files(&processed.albums[0]),
            vec![
                format!("{release}\\Disc 1\\03 - O Holy Night.flac"),
                format!("{release}\\Disc 1\\04 - Silent Night.flac"),
                format!("{release}\\Disc 2\\03 - O Holy Night.flac"),
            ]
        );

        // Without lengths the two files cannot be told apart, and each track still takes one.
        let unknown: Vec<WantedTrack> = deluxe
            .iter()
            .map(|t| WantedTrack {
                duration_s: None,
                ..t.clone()
            })
            .collect();
        let processed = process_search_responses(
            &responses,
            "Marrow Choir",
            Some("Winter Songs"),
            &unknown,
            false,
        );
        let mut files = album_files(&processed.albums[0]);
        files.sort();
        files.dedup();
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn records_bounded_rejection_reasons_and_locked_only_peers() {
        use crate::matching::diagnostics::{
            REASON_DURATION_DELTA, REASON_LOCKED_ONLY, REASON_TITLE_MISMATCH,
        };
        use shared::download::MAX_TOP_REJECTED;

        let mk_file = |name: &str, length: Option<i32>| SearchResponseFile {
            filename: name.into(),
            size: 1,
            bit_rate: None,
            length,
            sample_rate: None,
            bit_depth: None,
            extension: None,
            is_variable_bit_rate: None,
        };
        let responses = vec![
            SearchResponse {
                username: "locked_peer".into(),
                files: vec![],
                has_free_upload_slot: false,
                upload_speed: 0,
                queue_length: 0,
                locked_file_count: 4,
            },
            SearchResponse {
                username: "bad_peer".into(),
                files: vec![
                    mk_file("Music/Other/01 - Interview.flac", Some(263)),
                    mk_file("Music/Other/02 - Commentary.flac", Some(263)),
                    mk_file("Music/Other/03 - Outro.flac", Some(263)),
                    mk_file("Music/Other/04 - Skit.flac", Some(263)),
                    mk_file("Music/Other/05 - Bonus.flac", Some(263)),
                    mk_file("Music/Other/06 - O Holy Night.flac", Some(450)),
                ],
                has_free_upload_slot: true,
                upload_speed: 1,
                queue_length: 0,
                locked_file_count: 0,
            },
        ];
        let tracks = wanted(&[("O Holy Night", 263)]);
        let processed = process_search_responses(
            &responses,
            "Marrow Choir",
            Some("Winter Songs"),
            &tracks,
            false,
        );
        assert!(processed.albums.is_empty());
        assert_eq!(processed.rejection_counts.get(REASON_LOCKED_ONLY), Some(&1));
        assert_eq!(
            processed.rejection_counts.get(REASON_TITLE_MISMATCH),
            Some(&5)
        );
        assert_eq!(
            processed.rejection_counts.get(REASON_DURATION_DELTA),
            Some(&1)
        );
        assert_eq!(processed.top_rejected.len(), MAX_TOP_REJECTED);
        // Closest candidate (matched title, wrong duration) ranks first.
        assert_eq!(processed.top_rejected[0].reason, REASON_DURATION_DELTA);
        assert_eq!(processed.top_rejected[0].source, "bad_peer");
    }
}
