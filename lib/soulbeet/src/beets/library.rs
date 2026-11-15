//! Reads tracks back out of soulbeet's per-folder beets libraries.

use super::runner::{run_beet, RunEnd, RunLimits, RunSpec, Stream};
use super::BeetsSettings;
use crate::traits::ForgetOutcome;
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
const ID_FORMAT: &str = "@@sb@@$id";

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
    let rows = run_library(s, root, vec!["ls".into(), "-f".into(), LS_FORMAT.into()]).await?;
    Ok(rows
        .iter()
        .filter_map(|row| parse_row(row, &library))
        .collect())
}

/// Drops the item at exactly `file` from the library under `root`, leaving
/// the file on disk.
pub(crate) async fn forget_item(s: &BeetsSettings, root: &Path, file: &Path) -> ForgetOutcome {
    if !tokio::fs::try_exists(library_db(root))
        .await
        .unwrap_or(false)
    {
        return ForgetOutcome::NotInLibrary;
    }
    let mut query = OsString::from("path:");
    query.push(file);
    let ls = vec!["ls".into(), "-f".into(), ID_FORMAT.into(), query];
    let ids = match run_library(s, root, ls).await {
        Ok(ids) => ids,
        Err(e) => return ForgetOutcome::Failed(e),
    };
    let id: u64 = match ids.as_slice() {
        [] => return ForgetOutcome::NotInLibrary,
        [id] => match id.parse() {
            Ok(id) => id,
            Err(_) => {
                return ForgetOutcome::Failed(format!(
                    "beet ls gave an item id that is not a number: {id}"
                ))
            }
        },
        several => return ForgetOutcome::Several(several.len()),
    };
    // Without remove's own -d, beets leaves the file alone; -f skips the prompt.
    let remove = vec!["remove".into(), "-f".into(), format!("id:{id}").into()];
    match run_library(s, root, remove).await {
        Ok(_) => ForgetOutcome::Removed,
        Err(e) => ForgetOutcome::Failed(e),
    }
}

/// Runs `beet {command}` on the library under `root` and returns the text
/// after the row marker of each line that has one. Retried once when the
/// library db is locked.
async fn run_library(
    s: &BeetsSettings,
    root: &Path,
    command: Vec<OsString>,
) -> Result<Vec<String>, String> {
    let subcommand = command[0].to_string_lossy().into_owned();
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
        let mut args: Vec<OsString> = vec![
            "-c".into(),
            s.config.clone().into(),
            "-l".into(),
            library_db(root).into(),
            "-d".into(),
            root.into(),
        ];
        args.extend(command.iter().cloned());
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
            RunEnd::Exited(status) if status.success() => Ok(rows),
            RunEnd::Exited(status) => Err(format!(
                "beet {subcommand} failed ({status}): {}",
                outcome.tail
            )),
            RunEnd::Idle | RunEnd::Total => Err(format!("beet {subcommand} timed out")),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Logs each call of the fake `beet` as one line of args.txt.
    const LOG: &str = "echo \"$*\" >> args.txt";

    /// A folder holding an empty library db and a `beet` that runs `body`
    /// inside it.
    async fn fake_library(body: &str) -> (tempfile::TempDir, BeetsSettings) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(library_db(dir.path()), b"").unwrap();
        let script = format!("cd '{}'\n{body}", dir.path().display());
        let beet = crate::beets::tests::script(dir.path(), "beet", &script).await;
        let settings = BeetsSettings {
            config: dir.path().join("config.yaml"),
            beet,
            python: None,
            limits: RunLimits::default(),
        };
        (dir, settings)
    }

    #[tokio::test]
    async fn forgetting_one_item_removes_it_by_id_and_keeps_the_file() {
        let (dir, settings) = fake_library(&format!(
            "{LOG}\ncase \"$*\" in *' ls '*) echo '@@sb@@7';; esac"
        ))
        .await;
        let file = dir
            .path()
            .join("Marrow Choir/Winter Songs/03 O Holy Night.flac");
        assert_eq!(
            forget_item(&settings, dir.path(), &file).await,
            ForgetOutcome::Removed
        );
        let calls = std::fs::read_to_string(dir.path().join("args.txt")).unwrap();
        let calls: Vec<&str> = calls.lines().collect();
        let base = format!(
            "-c {} -l {} -d {}",
            settings.config.display(),
            super::library_db(dir.path()).display(),
            dir.path().display()
        );
        assert_eq!(
            calls,
            vec![
                format!("{base} ls -f @@sb@@$id path:{}", file.display()),
                format!("{base} remove -f id:7")
            ]
        );
    }

    #[tokio::test]
    async fn several_items_at_one_path_are_left_alone() {
        let (dir, settings) = fake_library(&format!("{LOG}\nprintf '@@sb@@7\\n@@sb@@9\\n'")).await;
        assert_eq!(
            forget_item(&settings, dir.path(), &dir.path().join("a.flac")).await,
            ForgetOutcome::Several(2)
        );
        assert!(!std::fs::read_to_string(dir.path().join("args.txt"))
            .unwrap()
            .contains(" remove "));
    }

    #[tokio::test]
    async fn an_id_that_is_not_a_number_removes_nothing() {
        let (dir, settings) = fake_library(&format!("{LOG}\necho '@@sb@@'")).await;
        assert!(matches!(
            forget_item(&settings, dir.path(), &dir.path().join("a.flac")).await,
            ForgetOutcome::Failed(_)
        ));
        assert!(!std::fs::read_to_string(dir.path().join("args.txt"))
            .unwrap()
            .contains(" remove "));
    }

    #[tokio::test]
    async fn forgetting_a_file_the_library_does_not_hold_reports_it() {
        let (dir, settings) = fake_library(LOG).await;
        assert_eq!(
            forget_item(&settings, dir.path(), &dir.path().join("a.flac")).await,
            ForgetOutcome::NotInLibrary
        );
        std::fs::remove_file(super::library_db(dir.path())).unwrap();
        assert_eq!(
            forget_item(&settings, dir.path(), &dir.path().join("a.flac")).await,
            ForgetOutcome::NotInLibrary
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("args.txt"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }
}
