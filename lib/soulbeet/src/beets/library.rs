//! Reads tracks back out of soulbeet's per-folder beets libraries.

use super::classify::LOCKED;
use super::runner::{run_beet, RunEnd, RunLimits, RunSpec, Stream};
use super::{BeetsSettings, NO_PYTHON};
use crate::traits::{ForgetOutcome, PruneOutcome, Relocation};
use serde::Deserialize;
use shared::failure::{Cause, Failure, FailureCode, RetryAdvice, Service, Stage};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use tracing::{info, warn};

/// Starts every `beet ls` row, so text a plugin prints before it (a prompt
/// without a newline) cannot be read as part of the row.
const ROW_MARKER: &str = "@@sb@@";
const ID_FORMAT: &str = "@@sb@@$id";

const PRUNE_PY: &str = include_str!("prune.py");
/// The command prune.py adds to beets.
const PRUNE_COMMAND: &str = "soulbeet-prune";

/// One item as `beet export -l -f jsonlines -i id,path` prints it: beets' formatted values,
/// so the id is text (beetsplug/export.py, beetsplug/info.py library_data_emitter).
#[derive(Deserialize)]
struct ExportedItem {
    id: String,
    path: PathBuf,
}

/// The file name of a library database, in its folder under
/// `BeetsSettings::libraries`, and at the root of the music folder itself in
/// earlier versions.
const LIBRARY_DB: &str = ".beets_library.db";

/// The files SQLite may keep beside a database: its rollback journal, which
/// after a crash holds what undoes a half-done write, or a write-ahead log and
/// its index.
const COMPANIONS: [&str; 3] = ["-journal", "-wal", "-shm"];

/// The folder that holds the library database of the music folder `root`:
/// `root`'s own path under `libraries`, so `/music/alice` has
/// `{libraries}/music/alice/.beets_library.db`. `root` is canonical, as the
/// import queue keys its libraries.
pub(crate) fn library_folder(libraries: &Path, root: &Path) -> PathBuf {
    let mut folder = libraries.to_path_buf();
    folder.extend(
        root.components()
            .filter(|c| matches!(c, Component::Normal(_))),
    );
    folder
}

/// The library database of the music folder `root`, in its `library_folder`.
/// One an earlier version kept at the root of the music folder is moved there
/// first: music folders are often on a network share, where SQLite cannot
/// lock its files. A folder that cannot be read is an error.
pub(crate) async fn library_db(s: &BeetsSettings, root: &Path) -> Result<PathBuf, String> {
    let folder = library_folder(&s.libraries, root);
    let db = folder.join(LIBRARY_DB);
    let earlier = root.join(LIBRARY_DB);
    if exists(&db).await? || !exists(&earlier).await? {
        return Ok(db);
    }
    move_database(&earlier, &folder, &db).await.map_err(|e| {
        format!(
            "Could not move the beets library database {} to {}: {e}",
            earlier.display(),
            db.display()
        )
    })?;
    info!(
        "Moved the beets library database {} to {}",
        earlier.display(),
        db.display()
    );
    Ok(db)
}

/// Whether `path` exists. A path that cannot be looked at is an error naming it.
async fn exists(path: &Path) -> Result<bool, String> {
    tokio::fs::try_exists(path)
        .await
        .map_err(|e| format!("Could not read {}: {e}", path.display()))
}

/// Moves the database `from`, with the files SQLite keeps beside it, to `to`
/// in `dir`, on another file system as a rule, so by copying. Each companion
/// is copied, or a stale one at `to` removed, before the database itself is
/// copied next to `to`, synced and renamed into place: `to` never holds part
/// of a database. `from` goes last, and one left behind is only logged, as
/// the copy at `to` is the one used from then on.
async fn move_database(from: &Path, dir: &Path, to: &Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    for suffix in COMPANIONS {
        let source = suffixed(from, suffix);
        let target = suffixed(to, suffix);
        match tokio::fs::try_exists(&source).await? {
            true => copy_synced(&source, &target).await?,
            false => remove_present(&target).await?,
        }
    }
    let partial = suffixed(to, ".partial");
    copy_synced(from, &partial).await?;
    tokio::fs::rename(&partial, to).await?;
    tokio::fs::File::open(dir).await?.sync_all().await?;
    let sources = COMPANIONS.map(|suffix| suffixed(from, suffix));
    for source in sources.iter().map(PathBuf::as_path).chain([from]) {
        if let Err(e) = remove_present(source).await {
            warn!(
                "Could not remove {} after moving it to {}: {e}",
                source.display(),
                to.display()
            );
        }
    }
    Ok(())
}

/// `path` with `suffix` after its file name, as SQLite names a database's journal.
fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

async fn copy_synced(from: &Path, to: &Path) -> std::io::Result<()> {
    tokio::fs::copy(from, to).await?;
    tokio::fs::File::open(to).await?.sync_all().await
}

/// Removes the file at `path`, when there is one.
async fn remove_present(path: &Path) -> std::io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    }
}

/// Whether the music folder `root` has a library db.
async fn has_library(s: &BeetsSettings, root: &Path) -> Result<bool, String> {
    exists(&library_db(s, root).await?).await
}

/// Drops the item at exactly `file` from the library under `root`, leaving
/// the file on disk.
pub(crate) async fn forget_item(s: &BeetsSettings, root: &Path, file: &Path) -> ForgetOutcome {
    match has_library(s, root).await {
        Ok(true) => {}
        Ok(false) => return ForgetOutcome::NotInLibrary,
        Err(e) => return ForgetOutcome::Failed(e),
    }
    let mut query = OsString::from("path:");
    query.push(file);
    let ls = vec!["ls".into(), "-f".into(), ID_FORMAT.into(), query];
    let ids = match run_library(s, root, Program::Beet(None), ls).await {
        Ok(ids) => ids,
        Err(e) => return ForgetOutcome::Failed(e.into()),
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
    match run_library(s, root, Program::Beet(None), remove).await {
        Ok(_) => ForgetOutcome::Removed,
        Err(e) => ForgetOutcome::Failed(e.into()),
    }
}

/// Drops from the library under `root` the items whose file is gone, leaving every file alone:
/// one run of prune.py, which lists the items, looks at their files and removes the gone ones,
/// none while no item has its file there.
pub(crate) async fn prune_items(s: &BeetsSettings, root: &Path) -> PruneOutcome {
    match has_library(s, root).await {
        Ok(true) => {}
        Ok(false) => return PruneOutcome::NoLibrary,
        Err(e) => return PruneOutcome::Failed(e),
    }
    let driver = Program::Driver {
        script: PRUNE_PY,
        limits: RunLimits::LIBRARY_PRUNE,
    };
    match run_library(s, root, driver, vec![PRUNE_COMMAND.into()]).await {
        Ok(rows) => pruned(&rows),
        Err(e) => PruneOutcome::Failed(e.into()),
    }
}

/// What prune.py's one summary row says.
fn pruned(rows: &[String]) -> PruneOutcome {
    let words: Vec<&str> = match rows {
        [row] => row.split(' ').collect(),
        _ => vec![],
    };
    let read = match words.as_slice() {
        ["pruned", removed, kept] => (removed.parse::<u64>().ok())
            .zip(kept.parse::<u64>().ok())
            .map(|(removed, kept)| PruneOutcome::Pruned { removed, kept }),
        ["unproven", items] => {
            (items.parse::<u64>().ok()).map(|items| PruneOutcome::Unproven { items })
        }
        _ => None,
    };
    read.unwrap_or_else(|| {
        PruneOutcome::Failed(format!(
            "prune.py gave no summary Soulbeet can read: {}",
            rows.join("\n")
        ))
    })
}

/// Moves the items with `item_ids` of the library under `root` to the paths beets gives them
/// (one `beet move` of the ids joined by " , ", which beets reads as OR), then reads where each
/// item's file is from beets' own JSON export, so no character of a file name can be taken for
/// a separator, also when the move failed, since beets may have moved some items before it
/// did. An item the export of all ids does not give cleanly is read again on its own. A path
/// inside `root` is given relative to it, any other as beets gives it. What went wrong, in this
/// order: beets' own move failure, each item moved out of `root`, each item whose path could
/// not be read (which beets may have moved); the failure (move_failure) names the first and
/// lists the others.
pub(crate) async fn move_items(s: &BeetsSettings, root: &Path, item_ids: &[u64]) -> Relocation {
    // `beet move` with no query moves every item of the library.
    if item_ids.is_empty() {
        return Relocation {
            now: vec![],
            failure: move_failure(vec![Problem {
                retry: RetryAdvice::NotRetryable,
                message: "Could not move the new copy into place: no items to move".into(),
                cause: None,
            }]),
        };
    }
    let mut command: Vec<OsString> = vec!["move".into()];
    command.extend(ids_query(item_ids));
    let moved = run_library(s, root, Program::Beet(None), command).await;
    let (now, unread) = read_paths(s, root, item_ids).await;
    let mut problems = Vec::new();
    if let Err(e) = &moved {
        problems.push(Problem {
            retry: RetryAdvice::SameSourceLater,
            message: format!("Could not move the new copy into place: {}", e.text),
            cause: e.cause(),
        });
    }
    for (id, path) in now.iter().filter(|(_, path)| inside(path, root).is_none()) {
        problems.push(Problem {
            retry: RetryAdvice::NotRetryable,
            message: format!(
                "beets moved item {id} to {}, which is not inside {}",
                path.display(),
                root.display()
            ),
            cause: None,
        });
    }
    for (id, why) in unread {
        let message = match moved {
            Err(_) => format!(
                "Soulbeet could not read where beets left item {id}: {}",
                why.text
            ),
            Ok(_) => format!(
                "beets may have moved the new copy, and Soulbeet could not read where item {id} \
                 is: {}",
                why.text
            ),
        };
        problems.push(Problem {
            retry: RetryAdvice::SameSourceLater,
            message,
            cause: why.cause,
        });
    }
    Relocation {
        now: now
            .into_iter()
            .map(|(id, path)| match inside(&path, root) {
                Some(relative) => (id, relative.to_path_buf()),
                None => (id, path),
            })
            .collect(),
        failure: move_failure(problems),
    }
}

/// One thing that went wrong in a move of new items: its retry advice, its text and its cause.
struct Problem {
    retry: RetryAdvice,
    message: String,
    cause: Option<Cause>,
}

/// The failure of a move that went wrong as `problems` say, None when nothing did: coded
/// import_beets_failed, it names the first problem, with its retry advice and its own cause.
/// When there are others, the cause is Soulful's list, one per line: the first problem's own
/// cause, then each other problem's text followed by its own cause, so no detail is dropped.
fn move_failure(problems: Vec<Problem>) -> Option<Failure> {
    let mut problems = problems.into_iter();
    let first = problems.next()?;
    let others: Vec<Problem> = problems.collect();
    let failure = Failure::new(
        FailureCode::ImportBeetsFailed,
        Stage::Import,
        first.retry,
        first.message,
    );
    let cause = match others.is_empty() {
        true => first.cause,
        false => {
            let mut lines: Vec<String> = first.cause.into_iter().map(|c| c.raw).collect();
            for other in others {
                lines.push(other.message);
                lines.extend(other.cause.map(|c| c.raw));
            }
            Some(Cause::new(Service::Soulful, None, lines.join("\n")))
        }
    };
    Some(Failure { cause, ..failure })
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

/// Why Soulful could not read where an item is: its text, and its cause, which is beets' own
/// output when beets failed, or the lines beets printed that Soulful could not take for the
/// item, as Soulful's own.
struct Unread {
    text: String,
    cause: Option<Cause>,
}

/// Where beets has each item of `item_ids`, read from `beet export` of them all, and, for each
/// that read does not give cleanly (beets failed, a line is not an id and a path, the id is not
/// one asked for or is given twice), from an export of that item alone. The items read, and
/// why each other one could not be.
async fn read_paths(
    s: &BeetsSettings,
    root: &Path,
    item_ids: &[u64],
) -> (Vec<(u64, PathBuf)>, Vec<(u64, Unread)>) {
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
        let lines = |rows: &[String]| {
            (!rows.is_empty()).then(|| Cause::new(Service::Soulful, None, rows.join("\n")))
        };
        let alone = match export(s, root, &[id]).await {
            Ok(rows) => match rows.as_slice() {
                [row] => exported_item(row)
                    .and_then(|(listed, path)| match listed == id {
                        true => Ok(path),
                        false => Err(format!("beet export gave item {listed} for item {id}")),
                    })
                    .map_err(|text| Unread {
                        text,
                        cause: lines(&rows),
                    }),
                rows => Err(Unread {
                    text: format!("beet export listed {} items for item {id}", rows.len()),
                    cause: lines(rows),
                }),
            },
            Err(e) => Err(Unread {
                cause: e.cause(),
                text: e.text,
            }),
        };
        match alone {
            Ok(path) => found.push((id, path)),
            Err(why) => unread.push((id, why)),
        }
    }
    found.sort_by_key(|(id, _)| *id);
    (found, unread)
}

/// `beet -p export export -l -f jsonlines -i id,path` of `item_ids`: one row per item.
async fn export(
    s: &BeetsSettings,
    root: &Path,
    item_ids: &[u64],
) -> Result<Vec<String>, RunFailure> {
    let mut command: Vec<OsString> = ["export", "-l", "-f", "jsonlines", "-i", "id,path"]
        .map(OsString::from)
        .to_vec();
    command.extend(ids_query(item_ids));
    run_library(s, root, Program::Beet(Some("export")), command).await
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

/// Why a beets library command failed: Soulful's text, which holds what beets printed, and
/// that output alone when beets exited with a failure after printing some.
#[derive(Debug)]
struct RunFailure {
    text: String,
    output: Option<String>,
}

impl RunFailure {
    /// beets' output as the cause, when beets printed any.
    fn cause(&self) -> Option<Cause> {
        self.output
            .as_ref()
            .map(|output| Cause::new(Service::Beets, None, output.clone()))
    }
}

impl From<RunFailure> for String {
    fn from(e: RunFailure) -> String {
        e.text
    }
}

/// What runs a library command.
enum Program<'a> {
    /// `beet` itself, loading only the named plugin when there is one.
    Beet(Option<&'a str>),
    /// A driver script, run by the Python that runs beet with beet's own arguments, within
    /// `limits`.
    Driver { script: &'a str, limits: RunLimits },
}

/// Runs `beet {command}` (or the driver with that command) on the library
/// under `root` and returns its rows: the text after the row marker of each
/// line that has one, or, with a plugin, every line, as `beet -p {plugin}`
/// loads that plugin alone and no other one can print to stdout. Retried once
/// when the library db is locked.
async fn run_library(
    s: &BeetsSettings,
    root: &Path,
    program: Program<'_>,
    command: Vec<OsString>,
) -> Result<Vec<String>, RunFailure> {
    let subcommand = command[0].to_string_lossy().into_owned();
    let db = library_db(s, root)
        .await
        .map_err(|text| RunFailure { text, output: None })?;
    let (executable, script, plugin, limits) = match program {
        Program::Beet(plugin) => (s.beet.as_path(), None, plugin, RunLimits::LIBRARY_LS),
        Program::Driver { script, limits } => match s.python.as_deref() {
            Some(python) => (python, Some(script), None, limits),
            None => {
                return Err(RunFailure {
                    text: NO_PYTHON.into(),
                    output: None,
                })
            }
        },
    };
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
            Stream::Stderr => locked |= line.contains(LOCKED),
        };
        let mut args: Vec<OsString> = match script {
            Some(script) => vec!["-P".into(), "-c".into(), script.into()],
            None => vec![],
        };
        // beets 2.10+ stores paths relative to the library directory, so -d
        // must name it or they resolve under the config's `directory`.
        args.extend([
            "-c".into(),
            s.config.clone().into(),
            "-l".into(),
            db.clone().into(),
            "-d".into(),
            root.into(),
        ]);
        if let Some(plugin) = plugin {
            args.extend(["-p".into(), plugin.into()]);
        }
        args.extend(command.iter().cloned());
        let spec = RunSpec {
            program: executable,
            args,
            limits,
        };
        let outcome = run_beet(spec, &mut on_line).await;

        if locked && !retried {
            retried = true;
            continue;
        }
        let failed = |text: String, output: Option<String>| Err(RunFailure { text, output });
        return match outcome.end {
            RunEnd::Exited(status) if status.success() => Ok(rows),
            RunEnd::Exited(status) => failed(
                format!("beet {subcommand} failed ({status}): {}", outcome.tail),
                (!outcome.tail.is_empty()).then_some(outcome.tail),
            ),
            RunEnd::Idle | RunEnd::Total => failed(format!("beet {subcommand} timed out"), None),
            RunEnd::SpawnFailed(e) => failed(format!("Could not start beets: {e}"), None),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Logs each call of the fake `beet` as one line of args.txt.
    const LOG: &str = "echo \"$*\" >> args.txt";

    /// A music folder with an empty library db and a `beet` that runs `body`
    /// inside it.
    async fn fake_library(body: &str) -> (tempfile::TempDir, BeetsSettings) {
        let dir = tempfile::tempdir().unwrap();
        let script = format!("cd '{}'\n{body}", dir.path().display());
        let beet = crate::beets::tests::script(dir.path(), "beet", &script).await;
        let settings = BeetsSettings {
            config: dir.path().join("config.yaml"),
            beet,
            python: None,
            libraries: dir.path().join("libraries"),
            limits: RunLimits::default(),
        };
        let db = db_of(&settings, dir.path());
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        std::fs::write(&db, b"").unwrap();
        (dir, settings)
    }

    /// Where `s` keeps the library db of the music folder `root`.
    fn db_of(s: &BeetsSettings, root: &Path) -> PathBuf {
        library_folder(&s.libraries, root).join(LIBRARY_DB)
    }

    #[test]
    fn a_library_db_lives_under_the_libraries_folder_at_its_music_folders_path() {
        assert_eq!(
            library_folder(
                Path::new("/data/beets-libraries"),
                Path::new("/music/alice")
            ),
            PathBuf::from("/data/beets-libraries/music/alice")
        );
        assert_eq!(
            library_folder(Path::new("/data/beets-libraries"), Path::new("/")),
            PathBuf::from("/data/beets-libraries")
        );
    }

    /// Settings whose libraries folder is `dir/libraries`, for a music folder
    /// at `dir/music`, which is returned.
    fn moving(dir: &Path) -> (BeetsSettings, PathBuf) {
        let root = dir.join("music");
        std::fs::create_dir(&root).unwrap();
        let settings = BeetsSettings {
            config: dir.join("config.yaml"),
            beet: dir.join("beet"),
            python: None,
            libraries: dir.join("libraries"),
            limits: RunLimits::default(),
        };
        (settings, root)
    }

    #[tokio::test]
    async fn a_db_at_the_root_of_the_music_folder_moves_to_the_libraries_folder_with_its_journal() {
        let dir = tempfile::tempdir().unwrap();
        let (settings, root) = moving(dir.path());
        let earlier = root.join(LIBRARY_DB);
        std::fs::write(&earlier, b"items").unwrap();
        // A hot journal undoes a write cut short, so it goes with the db.
        std::fs::write(suffixed(&earlier, "-journal"), b"undo").unwrap();
        // A log left from an earlier try, with no counterpart now, is not kept.
        let db = db_of(&settings, &root);
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        std::fs::write(suffixed(&db, "-wal"), b"stale").unwrap();

        assert_eq!(library_db(&settings, &root).await, Ok(db.clone()));

        assert_eq!(std::fs::read(&db).unwrap(), b"items");
        assert_eq!(std::fs::read(suffixed(&db, "-journal")).unwrap(), b"undo");
        assert!(!suffixed(&db, "-wal").exists());
        assert!(!suffixed(&db, ".partial").exists());
        assert!(!earlier.exists());
        assert!(!suffixed(&earlier, "-journal").exists());
        // Once moved, the db stays where it is.
        assert_eq!(library_db(&settings, &root).await, Ok(db.clone()));
        assert_eq!(std::fs::read(&db).unwrap(), b"items");
    }

    #[tokio::test]
    async fn a_db_in_the_libraries_folder_wins_over_one_left_at_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let (settings, root) = moving(dir.path());
        let db = db_of(&settings, &root);
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        std::fs::write(&db, b"moved").unwrap();
        std::fs::write(root.join(LIBRARY_DB), b"left").unwrap();

        assert_eq!(library_db(&settings, &root).await, Ok(db.clone()));
        assert_eq!(std::fs::read(&db).unwrap(), b"moved");
        assert_eq!(std::fs::read(root.join(LIBRARY_DB)).unwrap(), b"left");
    }

    #[tokio::test]
    async fn a_music_folder_without_a_db_gets_none_until_beets_makes_it() {
        let dir = tempfile::tempdir().unwrap();
        let (settings, root) = moving(dir.path());
        let db = db_of(&settings, &root);
        assert_eq!(library_db(&settings, &root).await, Ok(db));
        assert!(!settings.libraries.exists());
        assert_eq!(has_library(&settings, &root).await, Ok(false));
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
            db_of(&settings, dir.path()).display(),
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
            db_of(&settings, dir.path()).display(),
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
                "beets may have moved the new copy, and Soulbeet could not read where item 7 is: {why}"
            );
            assert!(message.starts_with(&prefix), "{message}");
            // The line beets printed is the cause, as Soulful's own failure to read it.
            let printed = line.replace("$PWD", &dir.path().display().to_string());
            assert_eq!(
                relocation.failure.unwrap().cause,
                Some(Cause::new(Service::Soulful, None, printed))
            );
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
            "beets may have moved the new copy, and Soulbeet could not read where item 9 is: \
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
        // beets' own output is the cause.
        assert_eq!(
            relocation.failure.unwrap().cause,
            Some(Cause::new(Service::Beets, None, "error: permission denied"))
        );
    }

    #[tokio::test]
    async fn an_item_beets_cannot_export_has_beets_output_as_its_cause() {
        let (dir, settings) = fake_library(&format!(
            "{LOG}\ncase \"$*\" in *' export '*) echo 'error: no such table: items' >&2; exit 1;; esac"
        ))
        .await;
        let relocation = move_items(&settings, dir.path(), &[7]).await;
        let message = failure_of(&relocation).2;
        assert!(
            message.starts_with(
                "beets may have moved the new copy, and Soulbeet could not read where item 7 is: \
                 beet export failed ("
            ),
            "{message}"
        );
        assert_eq!(
            relocation.failure.unwrap().cause,
            Some(Cause::new(
                Service::Beets,
                None,
                "error: no such table: items"
            ))
        );
    }

    /// A fake `beet` whose export lists item 7 outside the library and prints a line that is
    /// not an item for item 9, and whose move fails when `move_fails`.
    async fn moving_out_and_unreadable(move_fails: bool) -> (tempfile::TempDir, BeetsSettings) {
        let moving = match move_fails {
            true => "*' move '*) echo 'error: permission denied' >&2; exit 1;;",
            false => "",
        };
        fake_library(&format!(
            "{LOG}\ncase \"$*\" in\n\
             {moving}\n\
             *' export '*' , '*) echo '{{\"id\": \"7\",\"path\": \"/elsewhere/03.flac\"}}'; echo '9|||broken';;\n\
             *' export '*id:7) echo '{{\"id\": \"7\",\"path\": \"/elsewhere/03.flac\"}}';;\n\
             *' export '*id:9) echo '9|||broken';;\n\
             esac"
        ))
        .await
    }

    #[tokio::test]
    async fn every_problem_of_a_move_is_named_first_or_listed_in_the_cause() {
        let why_9 = exported_item("9|||broken").unwrap_err();
        let outside = |dir: &Path| {
            format!(
                "beets moved item 7 to /elsewhere/03.flac, which is not inside {}",
                dir.display()
            )
        };

        // beets' move failed, item 7 is outside the library and item 9 cannot be read: the
        // move failure is the message, and the cause holds beets' output, then the other two
        // in Soulful's words, item 9's with the line beets printed for it.
        let (dir, settings) = moving_out_and_unreadable(true).await;
        let relocation = move_items(&settings, dir.path(), &[7, 9]).await;
        assert_eq!(relocation.now, vec![(7, "/elsewhere/03.flac".into())]);
        let (code, retry, message) = failure_of(&relocation);
        assert_eq!(
            (code, retry),
            (FailureCode::ImportBeetsFailed, RetryAdvice::SameSourceLater)
        );
        assert!(
            message.starts_with("Could not move the new copy into place: beet move failed (")
                && message.ends_with("): error: permission denied"),
            "{message}"
        );
        assert_eq!(
            relocation.failure.unwrap().cause,
            Some(Cause::new(
                Service::Soulful,
                None,
                format!(
                    "error: permission denied\n{}\nSoulbeet could not read where beets left item \
                     9: {why_9}\n9|||broken",
                    outside(dir.path())
                )
            ))
        );

        // The move itself went through: the item outside the library is the message.
        let (dir, settings) = moving_out_and_unreadable(false).await;
        let relocation = move_items(&settings, dir.path(), &[7, 9]).await;
        assert_eq!(
            failure_of(&relocation),
            (
                FailureCode::ImportBeetsFailed,
                RetryAdvice::NotRetryable,
                outside(dir.path())
            )
        );
        assert_eq!(
            relocation.failure.unwrap().cause,
            Some(Cause::new(
                Service::Soulful,
                None,
                format!(
                    "beets may have moved the new copy, and Soulbeet could not read where item 9 \
                     is: {why_9}\n9|||broken"
                )
            ))
        );
    }

    #[tokio::test]
    async fn the_lines_beets_printed_for_each_unread_item_stay_in_the_cause() {
        // beets' move fails; its export gives item 7 twice and item 8 for item 9.
        let sevens = [
            r#"{"id": "7","path": "/lib/A/03.flac"}"#,
            r#"{"id": "7","path": "/lib/B/03.flac"}"#,
        ];
        let eight = r#"{"id": "8","path": "/lib/A/04.flac"}"#;
        let (dir, settings) = fake_library(&format!(
            "{LOG}\ncase \"$*\" in\n\
             *' move '*) echo 'error: permission denied' >&2; exit 1;;\n\
             *' export '*' , '*|*' export '*id:7) echo '{}'; echo '{}';;\n\
             *' export '*id:9) echo '{eight}';;\n\
             esac",
            sevens[0], sevens[1]
        ))
        .await;
        let relocation = move_items(&settings, dir.path(), &[7, 9]).await;
        assert_eq!(relocation.now, vec![]);
        assert!(
            failure_of(&relocation)
                .2
                .starts_with("Could not move the new copy into place: beet move failed ("),
            "{:?}",
            relocation.failure
        );
        assert_eq!(
            relocation.failure.unwrap().cause,
            Some(Cause::new(
                Service::Soulful,
                None,
                format!(
                    "error: permission denied\n\
                     Soulbeet could not read where beets left item 7: beet export listed 2 items \
                     for item 7\n{}\n{}\n\
                     Soulbeet could not read where beets left item 9: beet export gave item 8 for \
                     item 9\n{eight}",
                    sevens[0], sevens[1]
                )
            ))
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
        std::fs::remove_file(db_of(&settings, dir.path())).unwrap();
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

    /// `fake_library` with a python standing in for prune.py: it keeps the script it was given
    /// in script.py, logs the args after it as one line of args.txt and runs `body`.
    async fn fake_driver(body: &str) -> (tempfile::TempDir, BeetsSettings) {
        let (dir, mut settings) = fake_library("").await;
        let python = format!(
            "cd '{}'\nprintf '%s' \"$3\" > script.py\nshift 3\n{LOG}\n{body}",
            dir.path().display()
        );
        settings.python = Some(crate::beets::tests::script(dir.path(), "python", &python).await);
        (dir, settings)
    }

    #[tokio::test]
    async fn a_prune_is_one_run_of_prune_py_on_the_library() {
        let (dir, settings) = fake_driver(
            "echo 'soulbeet: dropped /m/a.flac'; echo 'soulbeet: dropped /m/b.flac'; \
             echo '@@sb@@pruned 2 5'",
        )
        .await;
        assert_eq!(
            prune_items(&settings, dir.path()).await,
            PruneOutcome::Pruned {
                removed: 2,
                kept: 5
            }
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("script.py")).unwrap(),
            PRUNE_PY
        );
        let calls = std::fs::read_to_string(dir.path().join("args.txt")).unwrap();
        assert_eq!(
            calls.lines().collect::<Vec<_>>(),
            vec![format!(
                "-c {} -l {} -d {} soulbeet-prune",
                settings.config.display(),
                db_of(&settings, dir.path()).display(),
                dir.path().display()
            )]
        );
    }

    #[tokio::test]
    async fn a_library_none_of_whose_files_is_there_is_left_whole() {
        let (dir, settings) = fake_driver("echo '@@sb@@unproven 2200'").await;
        assert_eq!(
            prune_items(&settings, dir.path()).await,
            PruneOutcome::Unproven { items: 2200 }
        );
    }

    #[tokio::test]
    async fn a_prune_without_a_library_or_a_summary_drops_nothing() {
        for printed in [
            "",
            "echo '@@sb@@pruned two 5'",
            "echo '@@sb@@pruned 1 5'; echo '@@sb@@pruned 1 5'",
        ] {
            let (dir, settings) = fake_driver(printed).await;
            let outcome = prune_items(&settings, dir.path()).await;
            assert!(
                matches!(&outcome, PruneOutcome::Failed(e)
                    if e.starts_with("prune.py gave no summary Soulbeet can read")),
                "{printed}: {outcome:?}"
            );
        }
        let (dir, settings) = fake_driver("echo '@@sb@@pruned 0 0'").await;
        std::fs::remove_file(db_of(&settings, dir.path())).unwrap();
        assert_eq!(
            prune_items(&settings, dir.path()).await,
            PruneOutcome::NoLibrary
        );
        assert!(!dir.path().join("args.txt").exists());
    }

    #[tokio::test]
    async fn a_prune_needs_the_python_of_beet() {
        let (dir, settings) = fake_library(LOG).await;
        assert_eq!(
            prune_items(&settings, dir.path()).await,
            PruneOutcome::Failed(NO_PYTHON.into())
        );
        assert!(!dir.path().join("args.txt").exists());
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
            libraries: dir.path().join("libraries"),
            limits: RunLimits::default(),
        };
        let expected = format!("Could not read {}: ", root.join(LIBRARY_DB).display());
        assert!(matches!(
            forget_item(&settings, &root, &root.join("a.flac")).await,
            ForgetOutcome::Failed(e) if e.starts_with(&expected)
        ));
    }
}
