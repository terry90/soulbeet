//! Where each Navidrome library is on this machine, and the file behind a song.

use super::path_unresolved;
use crate::core::database_failure;
use crate::models::navidrome_library::NavidromeLibraryRow;
use shared::failure::{Failure, Service, Stage};
use soulbeet::navidrome::models::NativeSong;
use sqlx::SqlitePool;
use std::path::{Component, Path, PathBuf};

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

/// The canonical file on this machine behind `song`. Its library's root is `music_path` for
/// the default library when set, else the learned root while Navidrome still reports the
/// library at the stored `navidrome_path`, else `song.library_path` itself; the file is that
/// root joined with `song.path`, which must be relative without `..`, and its size must equal
/// `song.size`. A link below the root is refused: Navidrome follows links by default
/// (conf/configuration.go:608) and lists a linked file as a song of its own
/// (scanner/walk_dir_tree.go:131-152), so that song is not the file the link leads to.
pub(crate) async fn local_file_in(
    db: &SqlitePool,
    song: &NativeSong,
    music_path: Option<&Path>,
) -> Result<PathBuf, Failure> {
    if song.missing {
        return Err(path_unresolved(
            "Navidrome lists the file of this song as missing",
        ));
    }
    let (root, relative) = located_in(db, song, music_path).await?;
    let local = root.join(relative);
    let no_file = |e: std::io::Error| {
        path_unresolved(format!("No file at {}", local.display())).with_cause(
            Service::Filesystem,
            None,
            e.to_string(),
        )
    };
    let canonical_root = tokio::fs::canonicalize(&root).await.map_err(no_file)?;
    let canonical = tokio::fs::canonicalize(&local).await.map_err(no_file)?;
    if canonical != canonical_root.join(relative) {
        return Err(path_unresolved(format!(
            "Navidrome's path for this song passes through a symbolic link: {}",
            song.path
        )));
    }
    let size = tokio::fs::metadata(&canonical)
        .await
        .map_err(no_file)?
        .len();
    if size != song.size {
        return Err(path_unresolved(format!(
            "{} has {size} bytes, Navidrome's song has {}",
            canonical.display(),
            song.size
        )));
    }
    Ok(canonical)
}

/// The path on this machine `song.path` names, as local_file_in finds its library's root,
/// not resolved and not checked.
pub(super) async fn local_path_in(
    db: &SqlitePool,
    song: &NativeSong,
    music_path: Option<&Path>,
) -> Result<PathBuf, Failure> {
    let (root, relative) = located_in(db, song, music_path).await?;
    Ok(root.join(relative))
}

/// `song`'s library root on this machine and its path below it (local_file_in).
async fn located_in<'s>(
    db: &SqlitePool,
    song: &'s NativeSong,
    music_path: Option<&Path>,
) -> Result<(PathBuf, &'s Path), Failure> {
    let Some(relative) = inside(&song.path) else {
        return Err(path_unresolved(format!(
            "Navidrome's path for this song is not inside its library: {}",
            song.path
        )));
    };
    let root = match music_path {
        Some(root) if song.library_id == DEFAULT_LIBRARY_ID => root.to_path_buf(),
        _ => match NavidromeLibraryRow::get_in(db, song.library_id)
            .await
            .map_err(|e| database_failure(Stage::Delete, e))?
        {
            Some(row) if row.navidrome_path == song.library_path => PathBuf::from(row.local_path),
            _ => PathBuf::from(&song.library_path),
        },
    };
    Ok((root, relative))
}

/// Learns where `song`'s library lives from `local`, the absolute path beets gave a file Soulful
/// imported: when `local` ends with `song.path` by component and its size is `song.size`, the
/// part before is the library's local root. Ok(false), nothing stored, when it does not match.
pub(crate) async fn learn_in(
    db: &SqlitePool,
    song: &NativeSong,
    local: &Path,
) -> Result<bool, String> {
    let Some(relative) = inside(&song.path) else {
        return Ok(false);
    };
    if !local.is_absolute() || !local.ends_with(relative) {
        return Ok(false);
    }
    let file = tokio::fs::metadata(local)
        .await
        .map_err(|e| format!("{}: {e}", local.display()))?;
    if !file.is_file() || file.len() != song.size {
        return Ok(false);
    }
    let kept = local.components().count() - relative.components().count();
    let root: PathBuf = local.components().take(kept).collect();
    let Some(local_path) = root.to_str() else {
        return Err(format!("{} is not valid UTF-8", root.display()));
    };
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
    use super::*;
    use crate::db::test_db;
    use crate::models::navidrome_library::NavidromeLibraryRow;
    use shared::failure::{FailureCode, RetryAdvice, Stage};
    use soulbeet::navidrome::models::NativeSong;
    use std::path::Path;

    fn native(library_path: &Path, path: &str, size: u64) -> NativeSong {
        NativeSong {
            id: "3sG8kPq0WmZ1xYbN4vTcDe".into(),
            path: path.into(),
            title: "O Holy Night".into(),
            library_id: 1,
            library_path: library_path.to_string_lossy().into_owned(),
            size,
            missing: false,
            artist: "Marrow Choir".into(),
            album: "Winter Songs".into(),
            album_artist: "Marrow Choir".into(),
            rating: 0,
            average_rating: None,
            mbz_recording_id: None,
        }
    }

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[tokio::test]
    async fn a_song_is_found_under_its_library_path_when_no_root_is_known() {
        let (db, dir) = (test_db::migrated().await, tempfile::tempdir().unwrap());
        let music = dir.path().join("music");
        write(&music.join("Marrow Choir/Winter Songs/03.flac"), b"audio");
        let found = local_file_in(
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
        assert!(learn_in(
            &db,
            &native(navi, "Marrow Choir/Winter Songs/03.flac", 5),
            &local.join("Marrow Choir/Winter Songs/03.flac")
        )
        .await
        .unwrap());
        let other = local_file_in(
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
        assert!(
            !learn_in(&db, &native(navi, "Other/Album/03.flac", 5), &local)
                .await
                .unwrap()
        );
        assert!(!learn_in(
            &db,
            &native(navi, "Marrow Choir/Winter Songs/03.flac", 6),
            &local
        )
        .await
        .unwrap());
        assert_eq!(NavidromeLibraryRow::get_in(&db, 1).await.unwrap(), None);
    }

    #[tokio::test]
    async fn the_music_path_override_wins_for_the_default_library_only() {
        let (db, dir) = (test_db::migrated().await, tempfile::tempdir().unwrap());
        let here = dir.path().join("here");
        write(&here.join("A/03.flac"), b"audio");
        let navi = Path::new("/srv/navidrome/music");
        let found = local_file_in(&db, &native(navi, "A/03.flac", 5), Some(&here))
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
        let found = local_file_in(&db, &native(&linked, "B/03.flac", 5), None)
            .await
            .unwrap();
        assert_eq!(
            found,
            std::fs::canonicalize(music.join("B/03.flac")).unwrap()
        );
    }
}
