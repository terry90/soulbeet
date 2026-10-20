//! Reads tracks back out of soulbeet's per-folder beets libraries.

use super::runner::{run_beet, RunEnd, RunLimits, RunSpec, Stream};
use super::BeetsSettings;
use shared::library::{DuplicateGroup, DuplicateReport, LibraryTrack};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::Path;
use tracing::{info, warn};

/// Starts every `beet ls` row, so text a plugin prints before it (a prompt
/// without a newline) cannot be read as part of the row.
const ROW_MARKER: &str = "@@sb@@";
const FIELD_SEP: &str = "|||";
const LS_FORMAT: &str = "@@sb@@$path|||$artist|||$title|||$album|||$albumartist";

pub(crate) fn library_db(root: &Path) -> std::path::PathBuf {
    root.join(".beets_library.db")
}

fn parse_row(row: &str, library: &str) -> Option<LibraryTrack> {
    let mut fields = row.split(FIELD_SEP);
    let mut next = || fields.next().map(str::to_string);
    Some(LibraryTrack {
        path: next()?,
        artist: next()?,
        title: next()?,
        album: next()?,
        album_artist: next()?,
        library_path: library.to_string(),
    })
}

/// Every track in the library under `root`. A folder without a library db has
/// no tracks.
pub(crate) async fn query_library(
    s: &BeetsSettings,
    root: &Path,
) -> Result<Vec<LibraryTrack>, String> {
    let db = library_db(root);
    if !tokio::fs::try_exists(&db).await.unwrap_or(false) {
        return Ok(Vec::new());
    }
    let library = db.to_string_lossy().into_owned();

    let mut retried = false;
    loop {
        let mut rows = Vec::new();
        let mut locked = false;
        let mut on_line = |stream: Stream, line: &str| match stream {
            Stream::Stdout => {
                if let Some((_, row)) = line.rsplit_once(ROW_MARKER) {
                    rows.push(row.to_string());
                }
            }
            Stream::Stderr => locked |= line.contains("database is locked"),
        };
        // beets 2.10+ stores paths relative to the library directory, so -d
        // must name it or they resolve under the config's `directory`.
        let args: Vec<OsString> = vec![
            "-c".into(),
            s.config.clone().into(),
            "-l".into(),
            db.clone().into(),
            "-d".into(),
            root.into(),
            "ls".into(),
            "-f".into(),
            LS_FORMAT.into(),
        ];
        let spec = RunSpec {
            program: &s.beet,
            args,
            limits: RunLimits::LIBRARY_LS,
        };
        let outcome = run_beet(spec, &mut on_line).await;

        if locked && !retried {
            retried = true;
            continue;
        }
        return match outcome.end {
            RunEnd::Exited(status) if status.success() => Ok(rows
                .iter()
                .filter_map(|row| parse_row(row, &library))
                .collect()),
            RunEnd::Exited(status) => Err(format!("beet ls failed ({status}): {}", outcome.tail)),
            RunEnd::Idle | RunEnd::Total => Err("beet ls timed out".to_string()),
            RunEnd::SpawnFailed(e) => Err(format!("Could not start beets: {e}")),
        };
    }
}

/// Tracks with the same artist and title in more than one of `roots`.
pub(crate) async fn find_duplicates_across_libraries(
    s: &BeetsSettings,
    roots: &[&Path],
) -> Result<DuplicateReport, String> {
    let mut all_tracks: Vec<LibraryTrack> = Vec::new();
    let mut libraries_scanned: Vec<String> = Vec::new();

    for root in roots {
        info!("Scanning library: {:?}", library_db(root));
        match query_library(s, root).await {
            Ok(tracks) => {
                info!("Found {} tracks in {:?}", tracks.len(), root);
                all_tracks.extend(tracks);
                libraries_scanned.push(root.to_string_lossy().to_string());
            }
            Err(e) => {
                warn!("Failed to scan library {:?}: {}", root, e);
            }
        }
    }

    // Grouped by (artist, title), compared case-insensitively.
    let mut track_groups: HashMap<(String, String), Vec<LibraryTrack>> = HashMap::new();
    for track in all_tracks {
        let key = (
            track.artist.to_lowercase().trim().to_string(),
            track.title.to_lowercase().trim().to_string(),
        );
        track_groups.entry(key).or_default().push(track);
    }

    let duplicates: Vec<DuplicateGroup> = track_groups
        .into_iter()
        .filter(|(_, tracks)| {
            let unique_libs: HashSet<_> = tracks.iter().map(|t| &t.library_path).collect();
            unique_libs.len() > 1
        })
        .map(|((artist, title), tracks)| {
            // Shown with the first track's original case.
            let display_artist = tracks.first().map(|t| t.artist.clone()).unwrap_or(artist);
            let display_title = tracks.first().map(|t| t.title.clone()).unwrap_or(title);
            DuplicateGroup {
                artist: display_artist,
                title: display_title,
                tracks,
            }
        })
        .collect();

    let total_duplicate_tracks: usize = duplicates.iter().map(|g| g.tracks.len()).sum();

    Ok(DuplicateReport {
        duplicates,
        total_duplicate_tracks,
        libraries_scanned,
    })
}
