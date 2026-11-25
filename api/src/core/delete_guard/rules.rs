//! Rules 1, 2 and 5 of design section 13: the file is in the caller's folder, it is a track,
//! and no unfinished job wants it.

use super::{in_use, not_a_track, outside_library};
use crate::auth::UserId;
use crate::core::database_failure;
use crate::folder_policy::PolicyFolder;
use crate::ledger::{store, JobState};
use shared::download::DownloadQuery;
use shared::failure::{Failure, Stage};
use shared::recommendation::normalize_for_matching;
use soulbeet::navidrome::models::NativeSong;
use sqlx::SqlitePool;
use std::path::{Component, Path};
use tracing::info;

/// Rule 1: the canonical deepest registered folder that holds the canonical file `path`
/// strictly, which `actor` must own. When folders of several users tie as the deepest, each of
/// them owns the file (D7). Another user's folder that does not resolve, and whose stored path
/// does not show `path` outside it, may hold the file, so it is kept; the refusal names neither
/// that folder nor its owner, the log does.
pub(crate) fn owning_folders<'a>(
    path: &Path,
    folders: &'a [PolicyFolder],
    actor: &UserId,
) -> Result<&'a Path, Failure> {
    let actor = actor.to_string();
    for folder in folders.iter().filter(|f| f.user_id != actor) {
        if let Err(error) = &folder.path {
            if !lexically_outside(path, &folder.stored_path) {
                info!(
                    "{} may be inside {} of user {}, which cannot be resolved: {error}",
                    path.display(),
                    folder.stored_path.display(),
                    folder.user_id
                );
                return Err(outside_library(
                    "The file may be inside a folder of another user",
                ));
            }
        }
    }
    let holding: Vec<(&PolicyFolder, &Path)> = folders
        .iter()
        .filter_map(|f| {
            let folder = f.path.as_deref().ok()?;
            (path != folder && path.starts_with(folder)).then_some((f, folder))
        })
        .collect();
    let deepest = holding
        .iter()
        .map(|(_, folder)| folder.components().count())
        .max();
    holding
        .into_iter()
        .find(|(f, folder)| Some(folder.components().count()) == deepest && f.user_id == actor)
        .map(|(_, folder)| folder)
        .ok_or_else(|| outside_library("The file is not inside one of your library folders"))
}

/// The ids of every registered folder, of any user, that holds the canonical file `path`
/// strictly, or does not resolve and whose stored path does not show `path` outside it: the
/// folders a job importing into could write `path` in.
pub(crate) fn holding_folders<'a>(path: &Path, folders: &'a [PolicyFolder]) -> Vec<&'a str> {
    folders
        .iter()
        .filter(|f| match &f.path {
            Ok(folder) => path != folder && path.starts_with(folder),
            Err(_) => !lexically_outside(path, &f.stored_path),
        })
        .map(|f| f.id.as_str())
        .collect()
}

/// Whether the stored folder path `stored` shows, compared as written, that
/// `file` is not inside it. A relative path or one with `..` shows nothing.
fn lexically_outside(file: &Path, stored: &Path) -> bool {
    stored.is_absolute()
        && !stored.components().any(|c| c == Component::ParentDir)
        && !file.starts_with(stored)
}

/// Rule 2: the canonical `path`, a regular file (local_file_in), has an audio extension.
pub(crate) fn audio_file(path: &Path) -> Result<(), Failure> {
    if !soulbeet::audio::is_audio_path(path) {
        return Err(not_a_track("The file is not an audio file"));
    }
    Ok(())
}

/// Rule 5: no job in one of `folder_ids` whose state is not terminal wants `song` (D16). Jobs
/// in `exempt` do not count. A state JobState::parse does not know, or a `wanted` that does
/// not parse, counts as wanting it.
pub(crate) async fn not_in_use_in(
    db: &SqlitePool,
    folder_ids: &[&str],
    song: &NativeSong,
    exempt: &[String],
) -> Result<(), Failure> {
    let jobs = store::open_wanted_in(db, folder_ids)
        .await
        .map_err(|e| database_failure(Stage::Delete, e))?;
    for (id, state, wanted) in jobs {
        if exempt.contains(&id) {
            continue;
        }
        let wants_it = JobState::parse(&state).is_none()
            || serde_json::from_str::<DownloadQuery>(&wanted)
                .map_or(true, |wanted| wants(&wanted, song));
        if wants_it {
            return Err(in_use());
        }
    }
    Ok(())
}

/// Whether `wanted` may resolve to `song`: a track with the song's recording MBID (both
/// non-empty) or the same artist and title as normalize_for_matching reads them, or an album
/// with the song's album title by its album artist or artist. Matching by name keeps more
/// files than the recording alone, never fewer.
fn wants(wanted: &DownloadQuery, song: &NativeSong) -> bool {
    let same = |a: &str, b: &str| normalize_for_matching(a) == normalize_for_matching(b);
    let recording = song.mbz_recording_id.as_deref().filter(|m| !m.is_empty());
    let a_track = wanted.tracks.iter().any(|track| {
        (recording.is_some() && track.mbid.as_deref() == recording)
            || (same(&track.artist, &song.artist) && same(&track.title, &song.title))
    });
    let the_album = wanted.album.as_ref().is_some_and(|album| {
        same(&album.title, &song.album)
            && (same(&album.artist, &song.album_artist) || same(&album.artist, &song.artist))
    });
    a_track || the_album
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::failure::FailureCode;
    use std::path::PathBuf;

    fn pf(user: &str, path: &Path, shared: bool) -> PolicyFolder {
        PolicyFolder {
            id: format!("{user}:{}", path.display()),
            user_id: user.to_string(),
            owner_is_admin: false,
            path: Ok(path.to_path_buf()),
            stored_path: path.to_path_buf(),
            shared,
        }
    }

    /// `pf` for a folder stored as `stored` that does not resolve.
    fn unresolved(user: &str, stored: &str) -> PolicyFolder {
        PolicyFolder {
            path: Err("Too many levels of symbolic links".to_string()),
            ..pf(user, Path::new(stored), false)
        }
    }

    fn owner(path: &Path, folders: &[PolicyFolder], actor: &str) -> Result<PathBuf, String> {
        owning_folders(path, folders, &UserId::new(actor))
            .map(Path::to_path_buf)
            .map_err(|f| {
                assert_eq!(
                    (f.code, &f.cause),
                    (FailureCode::DeleteOutsideLibrary, &None)
                );
                f.message
            })
    }

    fn not_yours() -> Result<PathBuf, String> {
        Err("The file is not inside one of your library folders".to_string())
    }

    fn may_be_others() -> Result<PathBuf, String> {
        Err("The file may be inside a folder of another user".to_string())
    }

    #[test]
    fn a_file_in_another_users_nested_folder_is_not_the_actors() {
        // Alice shared /music; Bob has /music/bob inside it.
        let tmp = Path::new("/srv");
        let folders = [
            pf("alice", &tmp.join("music"), true),
            pf("bob", &tmp.join("music/bob"), false),
        ];
        let bobs_file = tmp.join("music/bob/B/y.flac");
        let alices_file = tmp.join("music/alice/A/x.flac");
        assert_eq!(owner(&bobs_file, &folders, "alice"), not_yours());
        assert_eq!(
            owner(&bobs_file, &folders, "bob"),
            Ok(tmp.join("music/bob"))
        );
        assert_eq!(
            owner(&alices_file, &folders, "alice"),
            Ok(tmp.join("music"))
        );
        assert_eq!(owner(&alices_file, &folders, "bob"), not_yours());
        let elsewhere = tmp.join("elsewhere/z.flac");
        assert_eq!(owner(&elsewhere, &folders, "alice"), not_yours());
        // A folder never holds itself.
        let music = tmp.join("music");
        assert_eq!(owner(&music, &folders, "alice"), not_yours());
    }

    #[test]
    fn a_file_in_a_folder_two_users_share_is_each_ones() {
        let tmp = Path::new("/srv");
        let folders = [
            pf("alice", &tmp.join("music"), true),
            pf("bob", &tmp.join("music"), false),
        ];
        let file = tmp.join("music/x.flac");
        assert_eq!(owner(&file, &folders, "alice"), Ok(tmp.join("music")));
        assert_eq!(owner(&file, &folders, "bob"), Ok(tmp.join("music")));
        assert_eq!(owner(&file, &folders, "carol"), not_yours());
    }

    #[test]
    fn another_users_unresolvable_folder_protects_what_it_may_hold() {
        // Alice has /music; Bob's /music/bob cannot be resolved.
        let tmp = Path::new("/srv");
        let bobs = tmp.join("music/bob");
        let folders = [
            pf("alice", &tmp.join("music"), false),
            unresolved("bob", bobs.to_str().unwrap()),
        ];
        let bobs_file = bobs.join("B/y.flac");
        assert_eq!(owner(&bobs_file, &folders, "alice"), may_be_others());
        // Outside Bob's stored path, as written: still Alice's.
        assert_eq!(
            owner(&tmp.join("music/alice/A/x.flac"), &folders, "alice"),
            Ok(tmp.join("music"))
        );
        // Bob's unresolvable folder is no reason to keep a file from Bob;
        // this one sits in Alice's folder.
        assert_eq!(owner(&bobs_file, &folders, "bob"), not_yours());
    }

    #[test]
    fn a_stored_path_that_cannot_be_compared_protects_every_file() {
        let tmp = Path::new("/srv");
        let alices_file = tmp.join("music/alice/A/x.flac");
        for stored in [
            "music/bob".to_string(),
            format!("{}/music/alice/../bob", tmp.display()),
        ] {
            let folders = [
                pf("alice", &tmp.join("music"), false),
                unresolved("bob", &stored),
            ];
            assert_eq!(
                owner(&alices_file, &folders, "alice"),
                may_be_others(),
                "{stored}"
            );
        }
    }

    #[test]
    fn every_folder_that_may_hold_the_file_is_one_a_job_could_import_it_into() {
        let tmp = Path::new("/srv");
        let folders = [
            pf("alice", &tmp.join("music"), true),
            pf("bob", &tmp.join("music/alice"), false),
            pf("bob", &tmp.join("music/bob"), false),
            unresolved("carol", "/srv/music/alice/../carol"),
            unresolved("dave", "/srv/elsewhere"),
        ];
        assert_eq!(
            holding_folders(&tmp.join("music/alice/A/x.flac"), &folders),
            vec![
                "alice:/srv/music",
                "bob:/srv/music/alice",
                "carol:/srv/music/alice/../carol"
            ]
        );
    }
}
