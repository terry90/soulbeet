//! Where each Navidrome library is on this machine, and the file behind a song.

use super::unlink::FileId;
use super::{not_a_track, path_unresolved};
use crate::auth::UserId;
use crate::core::database_failure;
use crate::folder_policy::PolicyRows;
use crate::models::navidrome_library::NavidromeLibraryRow;
use shared::failure::{Failure, Service, Stage};
use soulbeet::navidrome::models::NativeSong;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use tracing::warn;

/// Navidrome's default library (model/library.go:31).
pub(crate) const DEFAULT_LIBRARY_ID: i64 = 1;

/// `path` when it names something inside a library: relative, with a name and no `..`.
pub(super) fn inside(path: &str) -> Option<&Path> {
    let path = Path::new(path);
    let mut named = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => named = true,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    named.then_some(path)
}

/// The canonical file on this machine behind `song`, and its identity as read for the size
/// check, the one compared at the unlink (rule 6). Its library's root is `music_path` for the
/// default library when set, else the learned root while Navidrome still reports the library
/// at the stored `navidrome_path`, else `song.library_path` itself; the file is that root
/// joined with `song.path`, which must be relative without `..`, and must be a regular file
/// whose size is `song.size`. A link below the root is refused: Navidrome follows links by
/// default (conf/configuration.go:608) and lists a linked file as a song of its own
/// (scanner/walk_dir_tree.go:131-152), so that song is not the file the link leads to.
pub(super) async fn local_file_in(
    db: &SqlitePool,
    song: &NativeSong,
    music_path: Option<&Path>,
) -> Result<(PathBuf, FileId), Failure> {
    file_under_root(db, song, music_path).await?
}

/// local_file_in, and when no file is found under the song's root and its library has no root
/// on this machine (rootless), learns the root from the files the folders of `owner` that
/// resolve may hold for the song (learn_from_folders) and looks again. With no proof the first
/// answer stands. A root that is here keeps the first answer: the file is gone, not the
/// library.
pub(super) async fn local_file_learning(
    db: &SqlitePool,
    song: &NativeSong,
    music_path: Option<&Path>,
    owner: &UserId,
) -> Result<(PathBuf, FileId), Failure> {
    let no_file = match file_under_root(db, song, music_path).await? {
        Ok(found) => return Ok(found),
        Err(no_file) => no_file,
    };
    if !rootless(db, song, music_path)
        .await
        .map_err(|e| database_failure(Stage::Delete, e))?
    {
        return Err(no_file);
    }
    let (folders, _) = PolicyRows::load_in(db)
        .await
        .map_err(|e| database_failure(Stage::Delete, e))?
        .inputs();
    let owner = owner.to_string();
    let owned: Vec<&Path> = folders
        .iter()
        .filter(|f| f.user_id == owner)
        .filter_map(|f| f.path.as_deref().ok())
        .collect();
    match learn_from_folders(db, song, &owned).await {
        Ok(true) => local_file_in(db, song, music_path).await,
        Ok(false) => Err(no_file),
        Err(e) => {
            warn!(
                "Could not learn where Navidrome library {} ({}) is on this machine from song {}: {e}",
                song.library_id, song.library_path, song.id
            );
            Err(no_file)
        }
    }
}

/// local_file_in's checks: Err for a refusal, Ok(Err) when no file is under the song's root.
async fn file_under_root(
    db: &SqlitePool,
    song: &NativeSong,
    music_path: Option<&Path>,
) -> Result<Result<(PathBuf, FileId), Failure>, Failure> {
    if song.missing {
        return Err(path_unresolved(
            "Navidrome lists the file of this song as missing",
        ));
    }
    let relative = relative_path(song)?;
    let root = root_in(db, song, music_path).await?;
    let local = root.join(relative);
    let no_file = |e: std::io::Error| {
        path_unresolved(format!("No file at {}", local.display())).with_cause(
            Service::Filesystem,
            None,
            e.to_string(),
        )
    };
    let canonical_root = match tokio::fs::canonicalize(&root).await {
        Ok(canonical) => canonical,
        Err(e) => return Ok(Err(no_file(e))),
    };
    let canonical = match tokio::fs::canonicalize(&local).await {
        Ok(canonical) => canonical,
        Err(e) => return Ok(Err(no_file(e))),
    };
    if canonical != canonical_root.join(relative) {
        return Err(path_unresolved(format!(
            "Navidrome's path for this song passes through a symbolic link: {}",
            song.path
        )));
    }
    let meta = match tokio::fs::symlink_metadata(&canonical).await {
        Ok(meta) => meta,
        Err(e) => return Ok(Err(no_file(e))),
    };
    if !meta.is_file() {
        return Err(not_a_track(format!(
            "{} is not a regular file",
            canonical.display()
        )));
    }
    if meta.len() != song.size {
        return Err(path_unresolved(format!(
            "{} has {} bytes, Navidrome's song has {}",
            canonical.display(),
            meta.len(),
            song.size
        )));
    }
    Ok(Ok((canonical, FileId::of(&meta))))
}

/// Whether `song`'s path on this machine, found as local_file_in finds it, is the canonical
/// file `path`: its library's canonical root joined with `song.path`, or that path resolved.
/// A song whose path is not inside its library, or whose root is not on this machine, names
/// no file here.
pub(crate) async fn names_file(
    db: &SqlitePool,
    song: &NativeSong,
    music_path: Option<&Path>,
    path: &Path,
) -> Result<bool, Failure> {
    let Some(relative) = inside(&song.path) else {
        return Ok(false);
    };
    let root = root_in(db, song, music_path).await?;
    if let Ok(canonical_root) = tokio::fs::canonicalize(&root).await {
        if canonical_root.join(relative) == path {
            return Ok(true);
        }
    }
    Ok(tokio::fs::canonicalize(root.join(relative))
        .await
        .is_ok_and(|resolved| resolved == path))
}

/// What a song other than the checked one is to the checked file, on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Other {
    /// Another file, or none of the checked file's size.
    Different,
    /// The checked file itself.
    Same,
    /// Its path cannot be compared or found here and it has the checked file's size, so it may
    /// be that file.
    Unknown,
}

/// What `other`, a song other than `song`, is to `path`, the canonical file local_file_in gave
/// for `song`: Same when Navidrome names the same path (its library path joined with its path)
/// or its path on this machine (local_file_in's root) resolves to `path`. When its path is not
/// inside its library it is Unknown. When its path does not resolve here and its size is the
/// file's it is Unknown if its library's root is not on this machine either (a library Soulful
/// does not see), or if one of the two songs' paths ends with the other's (the sign of a root
/// learned from another file: under it, the file's own song points one folder level too deep).
/// Anything else is Different.
pub(super) async fn other_in(
    db: &SqlitePool,
    music_path: Option<&Path>,
    song: &NativeSong,
    path: &Path,
    other: &NativeSong,
) -> Result<Other, Failure> {
    let Some(relative) = inside(&other.path) else {
        return Ok(Other::Unknown);
    };
    let listed = Path::new(&song.library_path).join(&song.path);
    if Path::new(&other.library_path).join(relative) == listed {
        return Ok(Other::Same);
    }
    let root = root_in(db, other, music_path).await?;
    match tokio::fs::canonicalize(root.join(relative)).await {
        Ok(resolved) if resolved == path => return Ok(Other::Same),
        Ok(_) => return Ok(Other::Different),
        Err(_) if other.size != song.size => return Ok(Other::Different),
        Err(_) => {}
    }
    let mine = Path::new(&song.path);
    let unseen_root = tokio::fs::canonicalize(&root).await.is_err();
    Ok(
        match unseen_root || relative.ends_with(mine) || mine.ends_with(relative) {
            true => Other::Unknown,
            false => Other::Different,
        },
    )
}

/// `song.path` when it is relative without `..`.
fn relative_path(song: &NativeSong) -> Result<&Path, Failure> {
    inside(&song.path).ok_or_else(|| {
        path_unresolved(format!(
            "Navidrome's path for this song is not inside its library: {}",
            song.path
        ))
    })
}

/// What is at a path on this machine, as a library root.
enum Dir {
    Here,
    /// Nothing, or something that is not a directory.
    Absent,
    /// It cannot be told: why.
    Unreadable(std::io::Error),
}

/// What is at `path`, following links.
async fn dir_at(path: &Path) -> Dir {
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.is_dir() => Dir::Here,
        Ok(_) => Dir::Absent,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Dir::Absent
        }
        Err(e) => Dir::Unreadable(e),
    }
}

/// Whether `song`'s library has no root on this machine, the one case a deletion check may
/// learn one in: it is not the default library under `music_path`, the root stored for the
/// library at Navidrome's current path of it, if any, is no longer a directory here, and that
/// Navidrome path is not a directory here. A root that is here, or that cannot be read, stays
/// the library's root when a file under it is missing; a row stored for another Navidrome path
/// is not a root of the library as Navidrome reports it (root_in does not read it).
async fn rootless(
    db: &SqlitePool,
    song: &NativeSong,
    music_path: Option<&Path>,
) -> Result<bool, String> {
    if music_path.is_some() && song.library_id == DEFAULT_LIBRARY_ID {
        return Ok(false);
    }
    let stored = NavidromeLibraryRow::get_in(db, song.library_id)
        .await?
        .filter(|row| row.navidrome_path == song.library_path);
    if let Some(row) = stored {
        if !matches!(dir_at(Path::new(&row.local_path)).await, Dir::Absent) {
            return Ok(false);
        }
    }
    Ok(matches!(
        dir_at(Path::new(&song.library_path)).await,
        Dir::Absent
    ))
}

/// `song`'s library root on this machine, as local_file_in finds it.
async fn root_in(
    db: &SqlitePool,
    song: &NativeSong,
    music_path: Option<&Path>,
) -> Result<PathBuf, Failure> {
    Ok(match music_path {
        Some(root) if song.library_id == DEFAULT_LIBRARY_ID => root.to_path_buf(),
        _ => match NavidromeLibraryRow::get_in(db, song.library_id)
            .await
            .map_err(|e| database_failure(Stage::Delete, e))?
        {
            Some(row) if row.navidrome_path == song.library_path => PathBuf::from(row.local_path),
            _ => PathBuf::from(&song.library_path),
        },
    })
}

/// Learns where Navidrome's libraries live from `local`, the absolute path beets gave a file
/// Soulful imported, and `listed`, every song the index lookup took for that file. A library
/// with more than one of them teaches nothing: all but one are other files of the same size
/// whose paths end like this one's, and the root each implies differs. The ids of the
/// libraries learned (learn_one). This is an import's learning (Learner::Import).
pub(crate) async fn learn_in(
    db: &SqlitePool,
    listed: &[NativeSong],
    local: &Path,
) -> Result<Vec<i64>, String> {
    let mut learned = Vec::new();
    for song in alone_in_library(listed) {
        if learn_one(db, song, local, Learner::Import).await? {
            learned.push(song.library_id);
        }
    }
    Ok(learned)
}

/// learn_in for a deletion check, from `local`, a file Soulful knows for the songs `listed`
/// (`music_path` is `NAVIDROME_MUSIC_PATH`): only a library that has no root on this machine
/// (rootless) learns one.
pub(super) async fn learn_in_check(
    db: &SqlitePool,
    listed: &[NativeSong],
    local: &Path,
    music_path: Option<&Path>,
) -> Result<Vec<i64>, String> {
    let mut learned = Vec::new();
    for song in alone_in_library(listed) {
        if rootless(db, song, music_path).await?
            && learn_one(db, song, local, Learner::Check).await?
        {
            learned.push(song.library_id);
        }
    }
    Ok(learned)
}

/// The songs of `listed` that are the only one of their library, by library id.
fn alone_in_library(listed: &[NativeSong]) -> Vec<&NativeSong> {
    let mut by_library: HashMap<i64, Vec<&NativeSong>> = HashMap::new();
    for song in listed {
        by_library.entry(song.library_id).or_default().push(song);
    }
    let mut alone: Vec<&NativeSong> = by_library
        .into_values()
        .filter_map(|songs| match songs.as_slice() {
            [song] => Some(*song),
            _ => None,
        })
        .collect();
    alone.sort_unstable_by_key(|song| song.library_id);
    alone
}

/// Who learns a library root, which decides what a stored root that is here gives way to.
#[derive(Clone, Copy)]
enum Learner {
    /// An import's index wait: a proof that contradicts a stored root forgets both.
    Import,
    /// A deletion check, for a rootless library only: a stored root that is here is never
    /// replaced or forgotten.
    Check,
}

/// Learns `song`'s library root from `local` (proven_root), storing it (store_root). Ok(true)
/// when a root is stored or already known.
async fn learn_one(
    db: &SqlitePool,
    song: &NativeSong,
    local: &Path,
    at: Learner,
) -> Result<bool, String> {
    match proven_root(song, local).await? {
        Some(proven) => store_root(db, song, &proven, at).await,
        None => Ok(false),
    }
}

/// Learns `song`'s library root, at a deletion check, from the files `folders` (canonical
/// folders of one user) may hold for it: each folder joined with `song.path`, and with what
/// follows each leading part of `song.path` the folder's path ends with (a folder inside
/// Navidrome's library). Those that are there are taken through proven_root, and only when
/// exactly one root is proven is it stored (store_root): two files that each prove a root may
/// each be the song's. Ok(true) when a root is stored or already known.
async fn learn_from_folders(
    db: &SqlitePool,
    song: &NativeSong,
    folders: &[&Path],
) -> Result<bool, String> {
    let Some(relative) = inside(&song.path) else {
        return Ok(false);
    };
    let parts: Vec<Component> = relative.components().collect();
    let mut proven: Vec<Proven> = Vec::new();
    for folder in folders {
        for skipped in 0..parts.len() {
            let leading: PathBuf = parts[..skipped].iter().collect();
            if !folder.ends_with(&leading) {
                continue;
            }
            let local = parts[skipped..]
                .iter()
                .fold(folder.to_path_buf(), |path, part| path.join(part));
            match tokio::fs::symlink_metadata(&local).await {
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    continue
                }
                _ => {}
            }
            if let Some(found) = proven_root(song, &local).await? {
                if !proven
                    .iter()
                    .any(|p| p.canonical_root == found.canonical_root)
                {
                    proven.push(found);
                }
            }
        }
    }
    match proven.as_slice() {
        [one] => store_root(db, song, one, Learner::Check).await,
        _ => Ok(false),
    }
}

/// A library root a file proves: as `local` is written, and canonical.
struct Proven {
    root: PathBuf,
    canonical_root: PathBuf,
    local: PathBuf,
}

/// The root of `song`'s library that `local` proves: when `local` is a regular file of
/// `song.size` bytes, with the modification time Navidrome read for `song` (`updated_at`), that
/// ends with `song.path` by whole components, the part before is the root, and the root's
/// canonical path joined with `song.path` must be the canonical `local` (no link below the
/// root, as local_file_in refuses one). The modification time tells the imported file from
/// another of its size whose path ends like it (another user's copy of the release, listed
/// before Navidrome lists the import), which would give a root one level too deep: a copy is
/// written at another time.
async fn proven_root(song: &NativeSong, local: &Path) -> Result<Option<Proven>, String> {
    let Some(relative) = inside(&song.path) else {
        return Ok(None);
    };
    if !local.is_absolute() || !local.ends_with(relative) {
        return Ok(None);
    }
    let unreadable = |e: std::io::Error| format!("{}: {e}", local.display());
    let file = tokio::fs::symlink_metadata(local)
        .await
        .map_err(unreadable)?;
    if !file.is_file() || file.len() != song.size {
        return Ok(None);
    }
    let modified = file.modified().map_err(unreadable)?;
    if chrono::DateTime::<chrono::Utc>::from(modified) != song.updated_at {
        return Ok(None);
    }
    let kept = local.components().count() - relative.components().count();
    let root: PathBuf = local.components().take(kept).collect();
    let canonical = tokio::fs::canonicalize(local).await.map_err(unreadable)?;
    let canonical_root = tokio::fs::canonicalize(&root).await.map_err(unreadable)?;
    if canonical_root.join(relative) != canonical {
        return Ok(None);
    }
    Ok(Some(Proven {
        root,
        canonical_root,
        local: local.to_path_buf(),
    }))
}

/// Stores `proven` as the root of `song`'s library. A root already stored for the same
/// Navidrome path is replaced only when it is no longer a directory here, and kept, with
/// nothing stored, when it cannot be read (Err). One that is here and resolves to another
/// directory is forgotten only by an import's proof: one of the two was learned from another
/// file, so both are forgotten and the conflict is logged; a check keeps it. Ok(true) when the
/// root is stored or already known.
async fn store_root(
    db: &SqlitePool,
    song: &NativeSong,
    proven: &Proven,
    at: Learner,
) -> Result<bool, String> {
    let Some(local_path) = proven.root.to_str() else {
        return Err(format!("{} is not valid UTF-8", proven.root.display()));
    };
    let stored = NavidromeLibraryRow::get_in(db, song.library_id).await?;
    if let Some(row) = stored.filter(|row| row.navidrome_path == song.library_path) {
        let unreadable = |e: std::io::Error| format!("{}: {e}", row.local_path);
        match dir_at(Path::new(&row.local_path)).await {
            Dir::Absent => {}
            Dir::Unreadable(e) => return Err(unreadable(e)),
            Dir::Here => {
                let known = tokio::fs::canonicalize(&row.local_path)
                    .await
                    .map_err(unreadable)?;
                if known == proven.canonical_root {
                    return Ok(true);
                }
                if let Learner::Import = at {
                    NavidromeLibraryRow::delete_in(db, song.library_id).await?;
                    warn!(
                        "Navidrome library {} ({}) was learned at {} on this machine, and {} puts \
                         it at {}: one of the two came from another file, so Soulful forgets both \
                         until an import shows it again",
                        song.library_id,
                        song.library_path,
                        row.local_path,
                        proven.local.display(),
                        local_path
                    );
                }
                return Ok(false);
            }
        }
    }
    NavidromeLibraryRow::upsert_in(
        db,
        &NavidromeLibraryRow {
            library_id: song.library_id,
            navidrome_path: song.library_path.clone(),
            local_path: local_path.to_string(),
        },
    )
    .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{mtime, set_mtime};
    use super::*;
    use crate::db::test_db;
    use crate::models::navidrome_library::NavidromeLibraryRow;
    use shared::failure::{FailureCode, RetryAdvice, Stage};
    use soulbeet::navidrome::models::NativeSong;
    use std::path::Path;

    /// No library learned.
    const NONE: Vec<i64> = Vec::new();

    /// As Navidrome sends it for a file `write` wrote.
    fn native(library_path: &Path, path: &str, size: u64) -> NativeSong {
        NativeSong {
            id: "3sG8kPq0WmZ1xYbN4vTcDe".into(),
            path: path.into(),
            title: "O Holy Night".into(),
            library_id: 1,
            library_path: library_path.to_string_lossy().into_owned(),
            size,
            missing: false,
            updated_at: mtime(),
            artist: "Marrow Choir".into(),
            album: "Winter Songs".into(),
            album_artist: "Marrow Choir".into(),
            rating: 0,
            average_rating: None,
            mbz_recording_id: None,
        }
    }

    /// Writes `bytes` at `path`, modified at test_support's mtime.
    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        set_mtime(path, mtime());
    }

    #[tokio::test]
    async fn a_song_is_found_under_its_library_path_when_no_root_is_known() {
        let (db, dir) = (test_db::migrated().await, tempfile::tempdir().unwrap());
        let music = dir.path().join("music");
        write(&music.join("Marrow Choir/Winter Songs/03.flac"), b"audio");
        let (found, _) = local_file_in(
            &db,
            &native(&music, "Marrow Choir/Winter Songs/03.flac", 5),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            found,
            std::fs::canonicalize(music.join("Marrow Choir/Winter Songs/03.flac")).unwrap()
        );
    }

    #[tokio::test]
    async fn a_learned_root_maps_navidromes_library_onto_this_machine_until_navidrome_moves_it() {
        let (db, dir) = (test_db::migrated().await, tempfile::tempdir().unwrap());
        let local = dir.path().join("data/music");
        write(&local.join("Marrow Choir/Winter Songs/03.flac"), b"audio");
        write(&local.join("Marrow Choir/Winter Songs/04.flac"), b"audio!");
        let navi = Path::new("/srv/navidrome/music");
        assert_eq!(
            learn_in(
                &db,
                &[native(navi, "Marrow Choir/Winter Songs/03.flac", 5)],
                &local.join("Marrow Choir/Winter Songs/03.flac")
            )
            .await
            .unwrap(),
            vec![1]
        );
        let (other, _) = local_file_in(
            &db,
            &native(navi, "Marrow Choir/Winter Songs/04.flac", 6),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            other,
            std::fs::canonicalize(local.join("Marrow Choir/Winter Songs/04.flac")).unwrap()
        );
        let moved = local_file_in(
            &db,
            &native(
                Path::new("/srv/other"),
                "Marrow Choir/Winter Songs/04.flac",
                6,
            ),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(moved.code, FailureCode::DeletePathUnresolved);
        assert_eq!(
            moved.message,
            "No file at /srv/other/Marrow Choir/Winter Songs/04.flac"
        );
    }

    #[tokio::test]
    async fn a_file_that_does_not_end_with_the_songs_path_or_has_another_size_teaches_nothing() {
        let (db, dir) = (test_db::migrated().await, tempfile::tempdir().unwrap());
        let local = dir.path().join("music/Marrow Choir/Winter Songs/03.flac");
        write(&local, b"audio");
        let navi = Path::new("/srv/navidrome/music");
        for song in [
            native(navi, "Other/Album/03.flac", 5),
            native(navi, "Marrow Choir/Winter Songs/03.flac", 6),
        ] {
            assert_eq!(learn_in(&db, &[song], &local).await.unwrap(), NONE);
        }
        assert_eq!(NavidromeLibraryRow::get_in(&db, 1).await.unwrap(), None);
    }

    #[tokio::test]
    async fn the_music_path_override_wins_for_the_default_library_only() {
        let (db, dir) = (test_db::migrated().await, tempfile::tempdir().unwrap());
        let here = dir.path().join("here");
        write(&here.join("A/03.flac"), b"audio");
        let navi = Path::new("/srv/navidrome/music");
        let (found, _) = local_file_in(&db, &native(navi, "A/03.flac", 5), Some(&here))
            .await
            .unwrap();
        assert_eq!(
            found,
            std::fs::canonicalize(here.join("A/03.flac")).unwrap()
        );
        let second = NativeSong {
            library_id: 2,
            ..native(navi, "A/03.flac", 5)
        };
        assert_eq!(
            local_file_in(&db, &second, Some(&here))
                .await
                .unwrap_err()
                .message,
            "No file at /srv/navidrome/music/A/03.flac"
        );
    }

    #[tokio::test]
    async fn navidromes_path_must_stay_inside_its_library_and_name_a_file_of_its_size() {
        let (db, dir) = (test_db::migrated().await, tempfile::tempdir().unwrap());
        let music = dir.path().join("music");
        write(&music.join("A/03.flac"), b"audio");
        for (song, message) in [
            (
                native(&music, "../bob/B/y.flac", 5),
                "Navidrome's path for this song is not inside its library: ../bob/B/y.flac"
                    .to_string(),
            ),
            (
                native(&music, "/etc/passwd", 5),
                "Navidrome's path for this song is not inside its library: /etc/passwd".to_string(),
            ),
            (
                NativeSong {
                    missing: true,
                    ..native(&music, "A/03.flac", 5)
                },
                "Navidrome lists the file of this song as missing".to_string(),
            ),
            (
                native(&music, "A/03.flac", 9),
                format!(
                    "{} has 5 bytes, Navidrome's song has 9",
                    std::fs::canonicalize(music.join("A/03.flac"))
                        .unwrap()
                        .display()
                ),
            ),
        ] {
            let f = local_file_in(&db, &song, None).await.unwrap_err();
            assert_eq!(
                (f.code, f.stage, f.retry, f.message.as_str()),
                (
                    FailureCode::DeletePathUnresolved,
                    Stage::Delete,
                    RetryAdvice::NotRetryable,
                    message.as_str()
                )
            );
        }
    }

    #[tokio::test]
    async fn a_song_reached_through_a_link_below_its_library_is_refused() {
        let (db, dir) = (test_db::migrated().await, tempfile::tempdir().unwrap());
        let music = dir.path().join("music");
        write(&music.join("B/03.flac"), b"audio");
        std::os::unix::fs::symlink(music.join("B"), music.join("A")).unwrap();
        let f = local_file_in(&db, &native(&music, "A/03.flac", 5), None)
            .await
            .unwrap_err();
        assert_eq!(
            (f.code, f.message.as_str()),
            (
                FailureCode::DeletePathUnresolved,
                "Navidrome's path for this song passes through a symbolic link: A/03.flac"
            )
        );
        // A library root that is itself a link resolves on both sides.
        let linked = dir.path().join("linked");
        std::os::unix::fs::symlink(&music, &linked).unwrap();
        let (found, _) = local_file_in(&db, &native(&linked, "B/03.flac", 5), None)
            .await
            .unwrap();
        assert_eq!(
            found,
            std::fs::canonicalize(music.join("B/03.flac")).unwrap()
        );
    }

    #[tokio::test]
    async fn a_directory_is_not_a_regular_file_and_a_file_has_the_identity_its_size_was_read_from()
    {
        let (db, dir) = (test_db::migrated().await, tempfile::tempdir().unwrap());
        let music = dir.path().join("music");
        write(&music.join("A/03.flac"), b"audio");
        std::fs::create_dir_all(music.join("B/dir.flac")).unwrap();
        let size = std::fs::metadata(music.join("B/dir.flac")).unwrap().len();
        let f = local_file_in(&db, &native(&music, "B/dir.flac", size), None)
            .await
            .unwrap_err();
        let canonical = std::fs::canonicalize(&music).unwrap();
        assert_eq!(
            (f.code, f.message),
            (
                FailureCode::DeleteNotATrack,
                format!(
                    "{} is not a regular file",
                    canonical.join("B/dir.flac").display()
                )
            )
        );
        let (path, id) = local_file_in(&db, &native(&music, "A/03.flac", 5), None)
            .await
            .unwrap();
        assert_eq!(id, FileId::of(&std::fs::symlink_metadata(&path).unwrap()));
    }

    /// Dave's shared folder `data/music` holds the release alice's `data/music/alice` holds too:
    /// her imported 03.flac, and dave's copy of the same size.
    fn shared_parent(dir: &Path) -> PathBuf {
        let alices = dir.join("data/music/alice/Artist/Album/03.flac");
        write(&alices, b"audio");
        write(&dir.join("data/music/Artist/Album/03.flac"), b"audio");
        alices
    }

    #[tokio::test]
    async fn two_songs_of_one_library_that_may_each_be_the_imported_file_teach_nothing() {
        let (db, dir) = (test_db::migrated().await, tempfile::tempdir().unwrap());
        let alices = shared_parent(dir.path());
        let navi = Path::new("/music");
        let daves_song = native(navi, "Artist/Album/03.flac", 5);
        let alices_song = native(navi, "alice/Artist/Album/03.flac", 5);
        assert_eq!(
            learn_in(&db, &[daves_song.clone(), alices_song.clone()], &alices)
                .await
                .unwrap(),
            NONE
        );
        assert_eq!(NavidromeLibraryRow::get_in(&db, 1).await.unwrap(), None);
        // Another library's song of the same file does not make library 1's ambiguous.
        let elsewhere = NativeSong {
            library_id: 2,
            library_path: "/kids".into(),
            ..native(navi, "Album/03.flac", 5)
        };
        assert_eq!(
            learn_in(&db, &[alices_song, elsewhere], &alices)
                .await
                .unwrap(),
            vec![1, 2]
        );
        assert_eq!(
            NavidromeLibraryRow::get_in(&db, 1)
                .await
                .unwrap()
                .map(|row| row.local_path),
            Some(dir.path().join("data/music").to_string_lossy().into_owned())
        );
    }

    #[tokio::test]
    async fn another_users_copy_listed_before_the_imported_file_teaches_nothing() {
        // Navidrome has not listed alice's import yet; dave's copy, written at another time, is
        // the one song of the library the lookup takes for it.
        let (db, dir) = (test_db::migrated().await, tempfile::tempdir().unwrap());
        let alices = shared_parent(dir.path());
        let written: chrono::DateTime<chrono::Utc> =
            "2026-10-03T21:14:09.583201447Z".parse().unwrap();
        set_mtime(&dir.path().join("data/music/Artist/Album/03.flac"), written);
        let daves_song = NativeSong {
            updated_at: written,
            ..native(Path::new("/music"), "Artist/Album/03.flac", 5)
        };
        assert_eq!(learn_in(&db, &[daves_song], &alices).await.unwrap(), NONE);
        assert_eq!(NavidromeLibraryRow::get_in(&db, 1).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_learned_root_is_not_replaced_by_another_that_resolves_and_both_are_forgotten() {
        let (db, dir) = (test_db::migrated().await, tempfile::tempdir().unwrap());
        let alices = shared_parent(dir.path());
        let navi = Path::new("/music");
        let alices_song = native(navi, "alice/Artist/Album/03.flac", 5);
        assert_eq!(
            learn_in(&db, std::slice::from_ref(&alices_song), &alices)
                .await
                .unwrap(),
            vec![1]
        );
        // Only dave's song listed: it would put library 1 at data/music/alice.
        assert_eq!(
            learn_in(&db, &[native(navi, "Artist/Album/03.flac", 5)], &alices)
                .await
                .unwrap(),
            NONE
        );
        assert_eq!(NavidromeLibraryRow::get_in(&db, 1).await.unwrap(), None);
        // The same root again, written another way, is the one already known.
        learn_in(&db, std::slice::from_ref(&alices_song), &alices)
            .await
            .unwrap();
        let dotted = dir.path().join("data/./music/alice/Artist/Album/03.flac");
        assert_eq!(
            learn_in(&db, &[alices_song], &dotted).await.unwrap(),
            vec![1]
        );
        assert_eq!(
            NavidromeLibraryRow::get_in(&db, 1)
                .await
                .unwrap()
                .map(|row| row.local_path),
            Some(dir.path().join("data/music").to_string_lossy().into_owned())
        );
    }

    #[tokio::test]
    async fn a_learned_root_that_is_gone_is_replaced() {
        let (db, dir) = (test_db::migrated().await, tempfile::tempdir().unwrap());
        let alices = shared_parent(dir.path());
        NavidromeLibraryRow::upsert_in(
            &db,
            &NavidromeLibraryRow {
                library_id: 1,
                navidrome_path: "/music".into(),
                local_path: dir.path().join("old-mount").to_string_lossy().into_owned(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            learn_in(
                &db,
                &[native(Path::new("/music"), "alice/Artist/Album/03.flac", 5)],
                &alices
            )
            .await
            .unwrap(),
            vec![1]
        );
        assert_eq!(
            NavidromeLibraryRow::get_in(&db, 1)
                .await
                .unwrap()
                .map(|row| row.local_path),
            Some(dir.path().join("data/music").to_string_lossy().into_owned())
        );
    }

    #[tokio::test]
    async fn a_learned_root_that_cannot_be_read_is_kept_and_nothing_is_stored() {
        use std::os::unix::fs::PermissionsExt;
        let (db, dir) = (test_db::migrated().await, tempfile::tempdir().unwrap());
        let alices = shared_parent(dir.path());
        let locked = dir.path().join("locked");
        std::fs::create_dir_all(locked.join("mount")).unwrap();
        let stored = NavidromeLibraryRow {
            library_id: 1,
            navidrome_path: "/music".into(),
            local_path: locked.join("mount").to_string_lossy().into_owned(),
        };
        NavidromeLibraryRow::upsert_in(&db, &stored).await.unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let learned = learn_in(
            &db,
            &[native(Path::new("/music"), "alice/Artist/Album/03.flac", 5)],
            &alices,
        )
        .await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            learned,
            Err(format!(
                "{}: {}",
                stored.local_path,
                std::io::Error::from_raw_os_error(13) // EACCES
            ))
        );
        assert_eq!(
            NavidromeLibraryRow::get_in(&db, 1).await.unwrap(),
            Some(stored)
        );
    }

    #[tokio::test]
    async fn a_file_reached_through_a_link_below_the_root_teaches_nothing() {
        let (db, dir) = (test_db::migrated().await, tempfile::tempdir().unwrap());
        let music = dir.path().join("music");
        write(&music.join("B/03.flac"), b"audio");
        std::os::unix::fs::symlink(music.join("B"), music.join("A")).unwrap();
        assert_eq!(
            learn_in(
                &db,
                &[native(Path::new("/music"), "A/03.flac", 5)],
                &music.join("A/03.flac")
            )
            .await
            .unwrap(),
            NONE
        );
        assert_eq!(NavidromeLibraryRow::get_in(&db, 1).await.unwrap(), None);
    }

    #[tokio::test]
    async fn learning_from_a_file_that_cannot_be_read_or_named_is_an_error() {
        use std::os::unix::ffi::OsStrExt;
        let (db, dir) = (test_db::migrated().await, tempfile::tempdir().unwrap());
        let gone = dir.path().join("music/A/03.flac");
        let navi = Path::new("/music");
        assert_eq!(
            learn_in(&db, &[native(navi, "A/03.flac", 5)], &gone).await,
            Err(format!(
                "{}: {}",
                gone.display(),
                std::io::Error::from_raw_os_error(2) // ENOENT
            ))
        );
        let odd = dir.path().join(std::ffi::OsStr::from_bytes(b"m\xffsic"));
        write(&odd.join("A/03.flac"), b"audio");
        assert_eq!(
            learn_in(&db, &[native(navi, "A/03.flac", 5)], &odd.join("A/03.flac")).await,
            Err(format!("{} is not valid UTF-8", odd.display()))
        );
        assert_eq!(NavidromeLibraryRow::get_in(&db, 1).await.unwrap(), None);
    }
}
