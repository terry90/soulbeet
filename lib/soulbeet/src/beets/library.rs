//! Reads tracks back out of soulbeet's per-folder beets libraries.

use super::runner::{run_beet, RunEnd, RunLimits, RunSpec, Stream};
use super::BeetsSettings;
use crate::traits::{ForgetOutcome, Relocation};
use serde::Deserialize;
use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage};
use shared::library::{DuplicateGroup, DuplicateReport, LibraryTrack};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use tracing::{info, warn};

/// Starts every `beet ls` row, so text a plugin prints before it (a prompt
/// without a newline) cannot be read as part of the row.
const ROW_MARKER: &str = "@@sb@@";
const FIELD_SEP: &str = "|||";
const LS_FORMAT: &str = "@@sb@@$path|||$artist|||$title|||$album|||$albumartist";
const ID_FORMAT: &str = "@@sb@@$id";

/// One item as `beet export -l -f jsonlines -i id,path` prints it: beets' formatted values,
/// so the id is text (beetsplug/export.py, beetsplug/info.py library_data_emitter).
#[derive(Deserialize)]
struct ExportedItem {
    id: String,
    path: PathBuf,
}

pub(crate) fn library_db(root: &Path) -> PathBuf {
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

/// Whether the folder `root` has a library db. A folder that cannot be read
/// is an error, not a folder without one.
async fn has_library(root: &Path) -> Result<bool, String> {
    let db = library_db(root);
    tokio::fs::try_exists(&db)
        .await
        .map_err(|e| format!("Could not read {}: {e}", db.display()))
}

/// Every track in the library under `root`. A folder without a library db has
/// no tracks.
pub(crate) async fn query_library(
    s: &BeetsSettings,
    root: &Path,
) -> Result<Vec<LibraryTrack>, String> {
    if !has_library(root).await? {
        return Ok(Vec::new());
    }
    let library = library_db(root).to_string_lossy().into_owned();
    let rows = run_library(
        s,
        root,
        None,
        vec!["ls".into(), "-f".into(), LS_FORMAT.into()],
    )
    .await?;
    Ok(rows
        .iter()
        .filter_map(|row| parse_row(row, &library))
        .collect())
}

/// Drops the item at exactly `file` from the library under `root`, leaving
/// the file on disk.
pub(crate) async fn forget_item(s: &BeetsSettings, root: &Path, file: &Path) -> ForgetOutcome {
    match has_library(root).await {
        Ok(true) => {}
        Ok(false) => return ForgetOutcome::NotInLibrary,
        Err(e) => return ForgetOutcome::Failed(e),
    }
    let mut query = OsString::from("path:");
    query.push(file);
    let ls = vec!["ls".into(), "-f".into(), ID_FORMAT.into(), query];
    let ids = match run_library(s, root, None, ls).await {
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
    match run_library(s, root, None, remove).await {
        Ok(_) => ForgetOutcome::Removed,
        Err(e) => ForgetOutcome::Failed(e),
    }
}

/// Moves the items with `item_ids` of the library under `root` to the paths beets gives them
/// (one `beet move` of the ids joined by " , ", which beets reads as OR), then reads where each
/// item's file is from beets' own JSON export, so no character of a file name can be taken for
/// a separator, also when the move failed, since beets may have moved some items before it
/// did. An item the export of all ids does not give cleanly is read again on its own. A path
/// inside `root` is given relative to it, any other as beets gives it. The failure, coded
/// import_beets_failed: beets' own move failure; else an item moved out of `root`; else an item
/// whose path could not be read, which beets may have moved.
pub(crate) async fn move_items(s: &BeetsSettings, root: &Path, item_ids: &[u64]) -> Relocation {
    // `beet move` with no query moves every item of the library.
    if item_ids.is_empty() {
        return Relocation {
            now: vec![],
            failure: Some(move_failed(
                RetryAdvice::NotRetryable,
                "Could not move the new copy into place: no items to move".into(),
                None,
            )),
        };
    }
    let mut command: Vec<OsString> = vec!["move".into()];
    command.extend(ids_query(item_ids));
    let moved = run_library(s, root, None, command).await;
    let (now, unread) = read_paths(s, root, item_ids).await;
    let outside = now.iter().find(|(_, path)| inside(path, root).is_none());
    let failure = match (&moved, outside, unread.first()) {
        (Err(e), _, None) => Some(move_failed(
            RetryAdvice::SameSourceLater,
            format!("Could not move the new copy into place: {e}"),
            Some(e.clone()),
        )),
        (Err(e), _, Some((id, read))) => Some(move_failed(
            RetryAdvice::SameSourceLater,
            format!(
                "Could not move the new copy into place: {e}; Soulful could not read where \
                 beets left item {id}: {read}"
            ),
            Some(e.clone()),
        )),
        (Ok(_), Some((id, path)), _) => Some(move_failed(
            RetryAdvice::NotRetryable,
            format!(
                "beets moved item {id} to {}, which is not inside {}",
                path.display(),
                root.display()
            ),
            None,
        )),
        (Ok(_), None, Some((id, read))) => Some(move_failed(
            RetryAdvice::SameSourceLater,
            format!(
                "beets may have moved the new copy, and Soulful could not read where item {id} \
                 is: {read}"
            ),
            Some(read.clone()),
        )),
        (Ok(_), None, None) => None,
    };
    Relocation {
        now: now
            .into_iter()
            .map(|(id, path)| match inside(&path, root) {
                Some(relative) => (id, relative.to_path_buf()),
                None => (id, path),
            })
            .collect(),
        failure,
    }
}

/// A move of a new copy that failed, as `message` says; `raw` is beets' own text.
fn move_failed(retry: RetryAdvice, message: String, raw: Option<String>) -> Failure {
    let failure = Failure::new(
        FailureCode::ImportBeetsFailed,
        Stage::Import,
        retry,
        message,
    );
    match raw {
        Some(raw) => failure.with_cause(Service::Beets, None, raw),
        None => failure,
    }
}

/// `id:a , id:b ...`: one beets query of `item_ids`, read as OR.
fn ids_query(item_ids: &[u64]) -> Vec<OsString> {
    let mut query: Vec<OsString> = Vec::new();
    for id in item_ids {
        if !query.is_empty() {
            query.push(",".into());
        }
        query.push(format!("id:{id}").into());
    }
    query
}

/// `path` relative to `root`, when it names something inside it.
fn inside<'a>(path: &'a Path, root: &Path) -> Option<&'a Path> {
    path.strip_prefix(root)
        .ok()
        .filter(|p| p.components().all(|c| matches!(c, Component::Normal(_))))
        .filter(|p| p.components().next().is_some())
}

/// Where beets has each item of `item_ids`, read from `beet export` of them all, and, for each
/// that read does not give cleanly (beets failed, a line is not an id and a path, the id is not
/// one asked for or is given twice), from an export of that item alone. The items read, and
/// why each other one could not be.
async fn read_paths(
    s: &BeetsSettings,
    root: &Path,
    item_ids: &[u64],
) -> (Vec<(u64, PathBuf)>, Vec<(u64, String)>) {
    let mut wanted = item_ids.to_vec();
    wanted.sort_unstable();
    wanted.dedup();
    let mut found: Vec<(u64, PathBuf)> = Vec::new();
    if let Ok(rows) = export(s, root, &wanted).await {
        let parsed: Vec<_> = rows.iter().map(|row| exported_item(row)).collect();
        for &id in &wanted {
            let mut paths = parsed
                .iter()
                .filter_map(|item| item.as_ref().ok())
                .filter(|(listed, _)| *listed == id);
            if let (Some((_, path)), None) = (paths.next(), paths.next()) {
                found.push((id, path.clone()));
            }
        }
        // A line that cannot be read may be any item's.
        if parsed.iter().any(|item| item.is_err()) {
            found.clear();
        }
    }
    let mut unread = Vec::new();
    for &id in &wanted {
        if found.iter().any(|(listed, _)| *listed == id) {
            continue;
        }
        let alone = match export(s, root, &[id]).await {
            Ok(rows) => match rows.as_slice() {
                [row] => exported_item(row).and_then(|(listed, path)| match listed == id {
                    true => Ok(path),
                    false => Err(format!("beet export gave item {listed} for item {id}")),
                }),
                rows => Err(format!(
                    "beet export listed {} items for item {id}",
                    rows.len()
                )),
            },
            Err(e) => Err(e),
        };
        match alone {
            Ok(path) => found.push((id, path)),
            Err(e) => unread.push((id, e)),
        }
    }
    found.sort_by_key(|(id, _)| *id);
    (found, unread)
}

/// `beet -p export export -l -f jsonlines -i id,path` of `item_ids`: one row per item.
async fn export(s: &BeetsSettings, root: &Path, item_ids: &[u64]) -> Result<Vec<String>, String> {
    let mut command: Vec<OsString> = ["export", "-l", "-f", "jsonlines", "-i", "id,path"]
        .map(OsString::from)
        .to_vec();
    command.extend(ids_query(item_ids));
    run_library(s, root, Some("export"), command).await
}

/// The id and path of one `beet export` row.
fn exported_item(row: &str) -> Result<(u64, PathBuf), String> {
    let item: ExportedItem = serde_json::from_str(row).map_err(|e| {
        format!("beet export gave a line that is not an item's id and path ({e}): {row}")
    })?;
    let id = item
        .id
        .parse()
        .map_err(|_| format!("beet export gave an item id that is not a number: {row}"))?;
    Ok((id, item.path))
}

/// Runs `beet {command}` on the library under `root` and returns its rows: the
/// text after the row marker of each line that has one, or, with `plugin`,
/// every line, as `beet -p {plugin}` loads that plugin alone and no other one
/// can print to stdout. Retried once when the library db is locked.
async fn run_library(
    s: &BeetsSettings,
    root: &Path,
    plugin: Option<&str>,
    command: Vec<OsString>,
) -> Result<Vec<String>, String> {
    let subcommand = command[0].to_string_lossy().into_owned();
    let mut retried = false;
    loop {
        let mut rows = Vec::new();
        let mut locked = false;
        let mut on_line = |stream: Stream, line: &str| match stream {
            Stream::Stdout if plugin.is_some() => {
                if !line.is_empty() {
                    rows.push(line.to_string());
                }
            }
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
        if let Some(plugin) = plugin {
            args.extend(["-p".into(), plugin.into()]);
        }
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

    /// A fake `beet` that logs each call and, for `beet export`, prints `lines`, each with
    /// `$PWD` standing for the library folder.
    async fn exporting(lines: &[&str]) -> (tempfile::TempDir, BeetsSettings) {
        let printed: String = lines.iter().map(|l| format!("{l}\\n")).collect();
        fake_library(&format!(
            "{LOG}\ncase \"$*\" in *' export '*) printf '{printed}' | sed \"s|\\$PWD|$PWD|g\";; esac"
        ))
        .await
    }

    #[tokio::test]
    async fn new_items_are_moved_by_id_in_one_call() {
        let (dir, settings) = exporting(&[
            r#"{"id": "7","path": "$PWD/A/B/03.flac"}"#,
            r#"{"id": "9","path": "$PWD/A/B/04.flac"}"#,
        ])
        .await;
        assert_eq!(
            move_items(&settings, dir.path(), &[7, 9]).await,
            Relocation {
                now: vec![(7, "A/B/03.flac".into()), (9, "A/B/04.flac".into())],
                failure: None
            }
        );
        let calls = std::fs::read_to_string(dir.path().join("args.txt")).unwrap();
        let base = format!(
            "-c {} -l {} -d {}",
            settings.config.display(),
            super::library_db(dir.path()).display(),
            dir.path().display()
        );
        assert_eq!(
            calls.lines().collect::<Vec<_>>(),
            vec![
                format!("{base} move id:7 , id:9"),
                format!("{base} -p export export -l -f jsonlines -i id,path id:7 , id:9"),
            ]
        );
    }

    #[tokio::test]
    async fn a_moved_path_is_read_whole_whatever_its_name_holds() {
        // As beets 2.11's export prints a name with a newline and the old row separator.
        let (dir, settings) =
            exporting(&[r#"{"id": "7","path": "$PWD/A/B/0\\n3|||x.flac"}"#]).await;
        assert_eq!(
            move_items(&settings, dir.path(), &[7]).await.now,
            vec![(7, "A/B/0\n3|||x.flac".into())]
        );
    }

    /// The failure of a relocation: its code, retry advice and message.
    fn failure_of(relocation: &Relocation) -> (FailureCode, RetryAdvice, String) {
        let f = relocation.failure.as_ref().expect("a failure");
        assert_eq!(f.stage, Stage::Import);
        (f.code, f.retry, f.message.clone())
    }

    #[tokio::test]
    async fn an_item_beets_moved_out_of_the_library_keeps_the_path_beets_gives_it() {
        for (line, moved_to) in [
            (
                r#"{"id": "7","path": "/elsewhere/03.flac"}"#,
                PathBuf::from("/elsewhere/03.flac"),
            ),
            (r#"{"id": "7","path": "$PWD/../03.flac"}"#, PathBuf::new()),
        ] {
            let (dir, settings) = exporting(&[line]).await;
            let moved_to = match moved_to.as_os_str().is_empty() {
                true => dir.path().join("../03.flac"),
                false => moved_to,
            };
            let relocation = move_items(&settings, dir.path(), &[7]).await;
            assert_eq!(relocation.now, vec![(7, moved_to.clone())]);
            assert_eq!(
                failure_of(&relocation),
                (
                    FailureCode::ImportBeetsFailed,
                    RetryAdvice::NotRetryable,
                    format!(
                        "beets moved item 7 to {}, which is not inside {}",
                        moved_to.display(),
                        dir.path().display()
                    )
                )
            );
        }
    }

    #[tokio::test]
    async fn an_item_whose_path_cannot_be_read_after_the_move_may_have_moved() {
        for (line, why) in [
            (
                r#"{"id": "x7","path": "$PWD/A/03.flac"}"#,
                "beet export gave an item id that is not a number: ",
            ),
            (
                "7|||$PWD/A/03.flac",
                "beet export gave a line that is not an item's id and path",
            ),
        ] {
            let (dir, settings) = exporting(&[line]).await;
            let relocation = move_items(&settings, dir.path(), &[7]).await;
            assert_eq!(relocation.now, vec![]);
            let (code, retry, message) = failure_of(&relocation);
            assert_eq!(
                (code, retry),
                (FailureCode::ImportBeetsFailed, RetryAdvice::SameSourceLater)
            );
            let prefix = format!(
                "beets may have moved the new copy, and Soulful could not read where item 7 is: {why}"
            );
            assert!(message.starts_with(&prefix), "{message}");
            // Read for all ids, then for item 7 alone.
            let calls = std::fs::read_to_string(dir.path().join("args.txt")).unwrap();
            assert_eq!(calls.lines().filter(|c| c.contains(" export ")).count(), 2);
        }
    }

    #[tokio::test]
    async fn moving_no_items_runs_no_beets() {
        let (dir, settings) = fake_library(LOG).await;
        let relocation = move_items(&settings, dir.path(), &[]).await;
        assert_eq!(
            failure_of(&relocation).2,
            "Could not move the new copy into place: no items to move"
        );
        assert!(!dir.path().join("args.txt").exists());
    }

    #[tokio::test]
    async fn an_item_the_export_of_all_does_not_list_is_read_alone() {
        // Every export prints item 7 only, so item 9 alone reads as item 7.
        let (dir, settings) = exporting(&[r#"{"id": "7","path": "$PWD/A/B/03.flac"}"#]).await;
        let relocation = move_items(&settings, dir.path(), &[7, 9]).await;
        assert_eq!(relocation.now, vec![(7, "A/B/03.flac".into())]);
        assert_eq!(
            failure_of(&relocation).2,
            "beets may have moved the new copy, and Soulful could not read where item 9 is: \
             beet export gave item 7 for item 9"
        );
    }

    #[tokio::test]
    async fn an_export_of_all_that_fails_is_read_again_item_by_item() {
        let (dir, settings) = fake_library(&format!(
            "{LOG}\ncase \"$*\" in\n\
             *' export '*' , '*) echo 'database is locked, give up' >&2; exit 1;;\n\
             *' export '*id:7) echo \"{{\\\"id\\\": \\\"7\\\",\\\"path\\\": \\\"$PWD/A/03.flac\\\"}}\";;\n\
             *' export '*id:9) echo \"{{\\\"id\\\": \\\"9\\\",\\\"path\\\": \\\"$PWD/A/04.flac\\\"}}\";;\n\
             esac"
        ))
        .await;
        assert_eq!(
            move_items(&settings, dir.path(), &[7, 9]).await,
            Relocation {
                now: vec![(7, "A/03.flac".into()), (9, "A/04.flac".into())],
                failure: None
            }
        );
    }

    #[tokio::test]
    async fn a_move_beets_fails_still_reads_where_the_items_are() {
        // beets moved item 7 before it failed on item 9.
        let (dir, settings) = fake_library(&format!(
            "{LOG}\ncase \"$*\" in\n\
             *' move '*) echo 'error: permission denied' >&2; exit 1;;\n\
             *' export '*) printf '%s\\n' \"{{\\\"id\\\": \\\"7\\\",\\\"path\\\": \\\"$PWD/A/03.flac\\\"}}\" \"{{\\\"id\\\": \\\"9\\\",\\\"path\\\": \\\"$PWD/old/04.flac\\\"}}\";;\n\
             esac"
        ))
        .await;
        let relocation = move_items(&settings, dir.path(), &[7, 9]).await;
        assert_eq!(
            relocation.now,
            vec![(7, "A/03.flac".into()), (9, "old/04.flac".into())]
        );
        let (code, retry, message) = failure_of(&relocation);
        assert_eq!(
            (code, retry),
            (FailureCode::ImportBeetsFailed, RetryAdvice::SameSourceLater)
        );
        assert!(
            message.starts_with("Could not move the new copy into place: beet move failed ("),
            "{message}"
        );
        assert_eq!(
            relocation.failure.unwrap().cause.map(|c| c.service),
            Some(Service::Beets)
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

    #[tokio::test]
    async fn a_library_folder_that_cannot_be_read_is_a_failure() {
        // A file where the folder should be: looking for the db inside it
        // fails with "Not a directory", which says nothing about the library.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("music");
        std::fs::write(&root, b"").unwrap();
        let settings = BeetsSettings {
            config: dir.path().join("config.yaml"),
            beet: dir.path().join("beet"),
            python: None,
            limits: RunLimits::default(),
        };
        let expected = format!("Could not read {}: ", library_db(&root).display());
        assert!(matches!(
            forget_item(&settings, &root, &root.join("a.flac")).await,
            ForgetOutcome::Failed(e) if e.starts_with(&expected)
        ));
        assert!(matches!(
            query_library(&settings, &root).await,
            Err(e) if e.starts_with(&expected)
        ));
    }
}
