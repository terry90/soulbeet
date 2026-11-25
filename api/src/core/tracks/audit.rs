//! Read-only library health audit comparing the caller's folders on disk against Navidrome's
//! indexed songs: untracked files on disk, missing files in Navidrome, split albums across
//! folders or libraries, songs missing MusicBrainz recording IDs, album directories missing cover
//! art, and tracks missing `.lrc`/`.txt` lyrics sidecars.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use shared::failure::{cut_json, Failure, FailureCode, RetryAdvice, Stage, TEXT_MAX_CHARS};
use shared::recommendation::normalize_for_matching;
use soulbeet::navidrome::models::NativeSong;
use soulbeet::navidrome::NavidromeClient;
use sqlx::SqlitePool;

use super::{find_sidecars, is_album_art};
use crate::auth::UserCtx;
use crate::core::database_failure;
use crate::core::delete_guard::paths::DEFAULT_LIBRARY_ID;
use crate::core::folders::{list_in, owned_in};
use crate::folder_policy::canonical_folder_path;
use crate::models::folder::Folder;
use crate::models::navidrome_library::NavidromeLibraryRow;

/// Default maximum number of sample entries returned per issue category.
pub const AUDIT_DEFAULT_SAMPLE: usize = 25;
/// Hard cap on `sample_limit`.
pub const AUDIT_MAX_SAMPLE: usize = 100;
/// Maximum directory depth walked beneath a library folder root.
const MAX_WALK_DEPTH: usize = 16;

#[derive(Debug, Clone, Serialize)]
pub struct AuditedFolder {
    pub folder_id: String,
    pub name: String,
    pub path: String,
    pub shared: bool,
    pub exists: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditSummary {
    pub total_songs_in_navidrome: usize,
    pub total_audio_files_on_disk: usize,
    pub untracked_on_disk: usize,
    pub missing_in_navidrome: usize,
    pub split_albums: usize,
    pub missing_mbids: usize,
    pub missing_cover_art: usize,
    pub missing_lyrics: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditOmitted {
    pub untracked_on_disk: usize,
    pub missing_in_navidrome: usize,
    pub split_albums: usize,
    pub missing_mbids: usize,
    pub missing_cover_art: usize,
    pub missing_lyrics: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct UntrackedFileIssue {
    pub path: String,
    pub folder_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MissingSongIssue {
    pub song_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub path: String,
    pub folder_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SplitAlbumIssue {
    pub album: String,
    pub artist: String,
    pub folders: Vec<String>,
    pub library_ids: Vec<i64>,
    pub track_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct MissingMbidIssue {
    pub song_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub folder_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MissingCoverArtIssue {
    pub album: String,
    pub artist: Option<String>,
    pub album_id: Option<String>,
    pub folder_id: String,
    pub directory: String,
    pub track_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct MissingLyricsIssue {
    pub song_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub folder_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditReport {
    pub sample_limit: usize,
    pub folders_audited: Vec<AuditedFolder>,
    pub summary: AuditSummary,
    pub truncated: bool,
    pub omitted: AuditOmitted,
    pub untracked_on_disk: Vec<UntrackedFileIssue>,
    pub missing_in_navidrome: Vec<MissingSongIssue>,
    pub split_albums: Vec<SplitAlbumIssue>,
    pub missing_mbids: Vec<MissingMbidIssue>,
    pub missing_cover_art: Vec<MissingCoverArtIssue>,
    pub missing_lyrics: Vec<MissingLyricsIssue>,
}

#[derive(Debug, Clone)]
struct ResolvedFolder {
    folder: Folder,
    canonical: Option<PathBuf>,
    exists: bool,
}

#[derive(Debug)]
struct WalkedDir {
    folder_id: String,
    audio_count: usize,
    has_art: bool,
}

#[derive(Debug, Default)]
struct DiskWalk {
    /// Canonical audio file path paired with the deepest audited folder holding it.
    audio_files: Vec<(PathBuf, String)>,
    /// Directories holding at least one audio file, keyed by canonical directory path.
    album_dirs: BTreeMap<PathBuf, WalkedDir>,
}

/// Runs a read-only library health audit for `ctx`, scoped to `folder_id` when given or across
/// all of `ctx`'s folders otherwise.
pub(crate) async fn audit_library(
    db: &SqlitePool,
    navi: &NavidromeClient,
    music_path: Option<&Path>,
    ctx: &UserCtx,
    folder_id: Option<&str>,
    sample_limit: usize,
) -> Result<AuditReport, Failure> {
    if sample_limit == 0 || sample_limit > AUDIT_MAX_SAMPLE {
        return Err(Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NotRetryable,
            format!("sample_limit must be between 1 and {AUDIT_MAX_SAMPLE}"),
        ));
    }

    let all_user_folders = list_in(db, ctx).await?;
    let audited_raw: Vec<Folder> = match folder_id {
        Some(fid) => vec![owned_in(db, ctx, fid).await?],
        None => all_user_folders.clone(),
    };

    let learned = NavidromeLibraryRow::all_in(db)
        .await
        .map_err(|e| database_failure(Stage::LibraryCheck, e))?;

    let page = navi
        .all_songs()
        .await
        .map_err(|e| e.failure(Stage::LibraryCheck, e.to_string()))?;

    let all_resolved: Vec<ResolvedFolder> = all_user_folders
        .into_iter()
        .map(resolve_folder)
        .collect();
    let audited_resolved: Vec<ResolvedFolder> = audited_raw
        .into_iter()
        .map(resolve_folder)
        .collect();

    // Sort deepest path first so nested folders match the most specific folder.
    let mut all_folders_by_depth: Vec<(PathBuf, String)> = all_resolved
        .iter()
        .filter_map(|f| Some((f.canonical.clone()?, f.folder.id.clone())))
        .collect();
    all_folders_by_depth.sort_by_key(|b| std::cmp::Reverse(b.0.components().count()));

    let mut audited_roots: Vec<(PathBuf, String)> = audited_resolved
        .iter()
        .filter(|f| f.exists)
        .filter_map(|f| Some((f.canonical.clone()?, f.folder.id.clone())))
        .collect();
    audited_roots.sort_by_key(|b| std::cmp::Reverse(b.0.components().count()));

    let audited_ids: HashSet<String> = audited_resolved
        .iter()
        .map(|f| f.folder.id.clone())
        .collect();

    let walk_roots = audited_roots.clone();
    let disk = tokio::task::spawn_blocking(move || walk_folders(&walk_roots))
        .await
        .map_err(|e| {
            Failure::new(
                FailureCode::SettingsUnreadable,
                Stage::LibraryCheck,
                RetryAdvice::SameSourceLater,
                format!("Library folder scan interrupted: {e}"),
            )
        })?;

    let music_path_buf = music_path.map(Path::to_path_buf);
    let mut root_cache: HashMap<(i64, String), PathBuf> = HashMap::new();

    // Track canonical paths listed by Navidrome and metadata per album directory.
    let mut tracked_paths: HashSet<PathBuf> = HashSet::new();
    let mut dir_meta: HashMap<PathBuf, (String, Option<String>, Option<String>)> = HashMap::new();

    let mut missing_in_navidrome: Vec<MissingSongIssue> = Vec::new();
    let mut missing_mbids: Vec<MissingMbidIssue> = Vec::new();
    let mut missing_lyrics: Vec<MissingLyricsIssue> = Vec::new();

    // Split album grouping: merge by normalized (album_artist|artist, album) AND by album_id.
    struct AlbumGroup {
        album: String,
        artist: String,
        folders: BTreeSet<String>,
        library_ids: BTreeSet<i64>,
        track_count: usize,
        touches_audited: bool,
    }
    let mut album_groups: Vec<AlbumGroup> = Vec::new();
    let mut group_by_key: HashMap<(String, String), usize> = HashMap::new();
    let mut group_by_album_id: HashMap<String, usize> = HashMap::new();

    let mut songs_in_scope = 0usize;

    for song in &page.songs {
        let expected_local = resolve_song_path(
            song,
            music_path_buf.as_deref(),
            &learned,
            &mut root_cache,
        );

        let user_folder_id = expected_local
            .as_deref()
            .and_then(|p| deepest_folder(p, &all_folders_by_depth));
        let in_audited_folder = user_folder_id
            .as_deref()
            .is_some_and(|id| audited_ids.contains(id));

        let canonical_existing = expected_local
            .as_deref()
            .filter(|_| !song.missing)
            .and_then(|p| std::fs::canonicalize(p).ok())
            .filter(|p| p.is_file());

        if let Some(ref existing) = canonical_existing {
            tracked_paths.insert(existing.clone());
            if let Some(parent) = existing.parent() {
                dir_meta.entry(parent.to_path_buf()).or_insert_with(|| {
                    let album = if !song.album.trim().is_empty() {
                        cut(&song.album)
                    } else {
                        parent
                            .file_name()
                            .map(|n| cut(&n.to_string_lossy()))
                            .unwrap_or_default()
                    };
                    let artist_raw = if !song.album_artist.trim().is_empty() {
                        &song.album_artist
                    } else {
                        &song.artist
                    };
                    let artist = (!artist_raw.trim().is_empty()).then(|| cut(artist_raw));
                    let album_id = (!song.album_id.is_empty()).then(|| song.album_id.clone());
                    (album, artist, album_id)
                });
            }
        }

        // Accumulate album groups across all caller songs so split albums spanning an audited
        // folder and another folder/library are always caught even when `folder_id` is set.
        if !song.album.trim().is_empty() {
            let eff_artist = if !song.album_artist.trim().is_empty() {
                &song.album_artist
            } else {
                &song.artist
            };
            let norm_key = (
                normalize_for_matching(eff_artist),
                normalize_for_matching(&song.album),
            );
            let idx = if !song.album_id.is_empty() {
                if let Some(&i) = group_by_album_id.get(&song.album_id) {
                    i
                } else if let Some(&i) = group_by_key.get(&norm_key) {
                    group_by_album_id.insert(song.album_id.clone(), i);
                    i
                } else {
                    let i = album_groups.len();
                    album_groups.push(AlbumGroup {
                        album: cut(&song.album),
                        artist: cut(eff_artist),
                        folders: BTreeSet::new(),
                        library_ids: BTreeSet::new(),
                        track_count: 0,
                        touches_audited: false,
                    });
                    group_by_key.insert(norm_key, i);
                    group_by_album_id.insert(song.album_id.clone(), i);
                    i
                }
            } else if let Some(&i) = group_by_key.get(&norm_key) {
                i
            } else {
                let i = album_groups.len();
                album_groups.push(AlbumGroup {
                    album: cut(&song.album),
                    artist: cut(eff_artist),
                    folders: BTreeSet::new(),
                    library_ids: BTreeSet::new(),
                    track_count: 0,
                    touches_audited: false,
                });
                group_by_key.insert(norm_key, i);
                i
            };
            let grp = &mut album_groups[idx];
            grp.track_count += 1;
            grp.library_ids.insert(song.library_id);
            if let Some(ref fid) = user_folder_id {
                grp.folders.insert(fid.clone());
            }
            if folder_id.is_none() || in_audited_folder {
                grp.touches_audited = true;
            }
        }

        // Scope filter for per-song checks: when `folder_id` is specified, inspect only songs
        // belonging to that folder.
        let in_scope = match folder_id {
            Some(_) => in_audited_folder,
            None => true,
        };
        if !in_scope {
            continue;
        }
        songs_in_scope += 1;

        // Check #2: missing_in_navidrome
        let file_missing = song.missing
            || (in_audited_folder && canonical_existing.is_none());
        if file_missing {
            missing_in_navidrome.push(MissingSongIssue {
                song_id: song.id.clone(),
                title: cut(&song.title),
                artist: cut(&song.artist),
                album: cut(&song.album),
                path: cut(&song.path),
                folder_id: user_folder_id.clone(),
            });
        }

        // Check #4: missing_mbids
        let has_mbid = song
            .mbz_recording_id
            .as_deref()
            .is_some_and(|m| !m.trim().is_empty());
        if !has_mbid {
            missing_mbids.push(MissingMbidIssue {
                song_id: song.id.clone(),
                title: cut(&song.title),
                artist: cut(&song.artist),
                album: cut(&song.album),
                folder_id: user_folder_id.clone(),
            });
        }

        // Check #6: missing_lyrics (only check files present on disk inside audited folders)
        if in_audited_folder {
            if let Some(ref local) = canonical_existing {
                if find_sidecars(local).track_files.is_empty() {
                    missing_lyrics.push(MissingLyricsIssue {
                        song_id: song.id.clone(),
                        title: cut(&song.title),
                        artist: cut(&song.artist),
                        album: cut(&song.album),
                        folder_id: user_folder_id.clone(),
                    });
                }
            }
        }
    }

    // Check #1: untracked_on_disk
    let mut untracked_on_disk: Vec<UntrackedFileIssue> = disk
        .audio_files
        .iter()
        .filter(|(path, _)| !tracked_paths.contains(path))
        .map(|(path, fid)| UntrackedFileIssue {
            path: cut(&path.to_string_lossy()),
            folder_id: fid.clone(),
        })
        .collect();
    untracked_on_disk.sort_by(|a, b| a.path.cmp(&b.path));

    // Check #3: split_albums
    let mut split_albums: Vec<SplitAlbumIssue> = album_groups
        .into_iter()
        .filter(|g| g.touches_audited && (g.folders.len() > 1 || g.library_ids.len() > 1))
        .map(|g| SplitAlbumIssue {
            album: g.album,
            artist: g.artist,
            folders: g.folders.into_iter().collect(),
            library_ids: g.library_ids.into_iter().collect(),
            track_count: g.track_count,
        })
        .collect();
    split_albums.sort_by(|a, b| a.artist.cmp(&b.artist).then_with(|| a.album.cmp(&b.album)));

    // Check #5: missing_cover_art
    let mut missing_cover_art: Vec<MissingCoverArtIssue> = disk
        .album_dirs
        .into_iter()
        .filter(|(_, info)| info.audio_count > 0 && !info.has_art)
        .map(|(dir, info)| {
            let (album, artist, album_id) = match dir_meta.get(&dir) {
                Some((al, ar, aid)) => (al.clone(), ar.clone(), aid.clone()),
                None => (
                    dir.file_name()
                        .map(|n| cut(&n.to_string_lossy()))
                        .unwrap_or_default(),
                    None,
                    None,
                ),
            };
            MissingCoverArtIssue {
                album,
                artist,
                album_id,
                folder_id: info.folder_id,
                directory: cut(&dir.to_string_lossy()),
                track_count: info.audio_count,
            }
        })
        .collect();
    missing_cover_art.sort_by(|a, b| a.directory.cmp(&b.directory));

    missing_in_navidrome.sort_by(|a, b| a.song_id.cmp(&b.song_id));
    missing_mbids.sort_by(|a, b| a.song_id.cmp(&b.song_id));
    missing_lyrics.sort_by(|a, b| a.song_id.cmp(&b.song_id));

    let summary = AuditSummary {
        total_songs_in_navidrome: songs_in_scope,
        total_audio_files_on_disk: disk.audio_files.len(),
        untracked_on_disk: untracked_on_disk.len(),
        missing_in_navidrome: missing_in_navidrome.len(),
        split_albums: split_albums.len(),
        missing_mbids: missing_mbids.len(),
        missing_cover_art: missing_cover_art.len(),
        missing_lyrics: missing_lyrics.len(),
    };

    let omitted = AuditOmitted {
        untracked_on_disk: untracked_on_disk.len().saturating_sub(sample_limit),
        missing_in_navidrome: missing_in_navidrome.len().saturating_sub(sample_limit),
        split_albums: split_albums.len().saturating_sub(sample_limit),
        missing_mbids: missing_mbids.len().saturating_sub(sample_limit),
        missing_cover_art: missing_cover_art.len().saturating_sub(sample_limit),
        missing_lyrics: missing_lyrics.len().saturating_sub(sample_limit),
    };

    untracked_on_disk.truncate(sample_limit);
    missing_in_navidrome.truncate(sample_limit);
    split_albums.truncate(sample_limit);
    missing_mbids.truncate(sample_limit);
    missing_cover_art.truncate(sample_limit);
    missing_lyrics.truncate(sample_limit);

    let truncated = omitted.untracked_on_disk > 0
        || omitted.missing_in_navidrome > 0
        || omitted.split_albums > 0
        || omitted.missing_mbids > 0
        || omitted.missing_cover_art > 0
        || omitted.missing_lyrics > 0;

    let folders_audited = audited_resolved
        .into_iter()
        .map(|f| AuditedFolder {
            folder_id: f.folder.id,
            name: cut(&f.folder.name),
            path: cut(&f
                .canonical
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(f.folder.path)),
            shared: f.folder.shared,
            exists: f.exists,
        })
        .collect();

    Ok(AuditReport {
        sample_limit,
        folders_audited,
        summary,
        truncated,
        omitted,
        untracked_on_disk,
        missing_in_navidrome,
        split_albums,
        missing_mbids,
        missing_cover_art,
        missing_lyrics,
    })
}

fn cut(s: &str) -> String {
    cut_json(s.to_string(), TEXT_MAX_CHARS)
}

fn resolve_folder(folder: Folder) -> ResolvedFolder {
    let canonical = canonical_folder_path(Path::new(&folder.path)).ok();
    let exists = canonical.as_ref().is_some_and(|p| p.is_dir());
    ResolvedFolder {
        folder,
        canonical,
        exists,
    }
}

fn deepest_folder(path: &Path, folders_by_depth: &[(PathBuf, String)]) -> Option<String> {
    folders_by_depth
        .iter()
        .find(|(root, _)| path != root && path.starts_with(root))
        .map(|(_, id)| id.clone())
}

fn relative_inside(path: &str) -> Option<&Path> {
    let p = Path::new(path);
    let mut named = false;
    for c in p.components() {
        match c {
            Component::Normal(_) => named = true,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    named.then_some(p)
}

fn resolve_song_path(
    song: &NativeSong,
    music_path: Option<&Path>,
    learned: &[NavidromeLibraryRow],
    root_cache: &mut HashMap<(i64, String), PathBuf>,
) -> Option<PathBuf> {
    let rel = relative_inside(&song.path)?;
    let key = (song.library_id, song.library_path.clone());
    let root = root_cache
        .entry(key)
        .or_insert_with(|| {
            let raw = if let Some(mp) = music_path {
                if song.library_id == DEFAULT_LIBRARY_ID {
                    mp.to_path_buf()
                } else {
                    learned_or_navidrome(song, learned)
                }
            } else {
                learned_or_navidrome(song, learned)
            };
            canonical_folder_path(&raw).unwrap_or(raw)
        })
        .clone();
    Some(root.join(rel))
}

fn learned_or_navidrome(song: &NativeSong, learned: &[NavidromeLibraryRow]) -> PathBuf {
    learned
        .iter()
        .find(|r| r.library_id == song.library_id && r.navidrome_path == song.library_path)
        .map(|r| PathBuf::from(&r.local_path))
        .unwrap_or_else(|| PathBuf::from(&song.library_path))
}

/// Recursively walks `roots` (canonical folder roots, deepest first), skipping symlinks, hidden
/// `.` entries, `Discovery/` staging subdirectories at folder roots, and directories deeper than
/// `MAX_WALK_DEPTH`.
fn walk_folders(roots: &[(PathBuf, String)]) -> DiskWalk {
    let mut result = DiskWalk::default();
    let mut visited_dirs: HashSet<PathBuf> = HashSet::new();

    for (root, _) in roots {
        walk_dir(root, 0, roots, &mut visited_dirs, &mut result);
    }

    result
}

fn walk_dir(
    dir: &Path,
    depth: usize,
    roots_by_depth: &[(PathBuf, String)],
    visited_dirs: &mut HashSet<PathBuf>,
    out: &mut DiskWalk,
) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    let Ok(canonical_dir) = std::fs::canonicalize(dir) else {
        return;
    };
    if !visited_dirs.insert(canonical_dir.clone()) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&canonical_dir) else {
        return;
    };

    let mut subdirs: Vec<PathBuf> = Vec::new();
    let mut audio_count = 0usize;
    let mut has_art = false;

    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            if depth == 0 && name_str == "Discovery" {
                continue;
            }
            subdirs.push(path);
        } else if ft.is_file() {
            if is_album_art(&name_str) {
                has_art = true;
            } else if soulbeet::audio::is_audio_path(&path) {
                audio_count += 1;
                if let Ok(canon_file) = std::fs::canonicalize(&path) {
                    if let Some(fid) = deepest_folder(&canon_file, roots_by_depth) {
                        out.audio_files.push((canon_file, fid));
                    }
                }
            }
        }
    }

    if audio_count > 0 {
        let fid = deepest_folder(&canonical_dir, roots_by_depth).or_else(|| {
            roots_by_depth
                .iter()
                .find(|(r, _)| *r == canonical_dir)
                .map(|(_, id)| id.clone())
        });
        if let Some(folder_id) = fid {
            out.album_dirs.insert(
                canonical_dir,
                WalkedDir {
                    folder_id,
                    audio_count,
                    has_art,
                },
            );
        }
    }

    for sub in subdirs {
        walk_dir(&sub, depth + 1, roots_by_depth, visited_dirs, out);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::core::delete_guard::test_support::{mtime, Library};
    use crate::core::test_ctx;
    use crate::test_navidrome::NavidromeStub;

    #[allow(clippy::too_many_arguments)]
    fn song_json(
        id: &str,
        path: &str,
        title: &str,
        artist: &str,
        album: &str,
        album_id: &str,
        library_id: i64,
        library_path: &str,
        mbid: Option<&str>,
        missing: bool,
    ) -> serde_json::Value {
        json!({
            "id": id,
            "path": path,
            "title": title,
            "artist": artist,
            "album": album,
            "albumArtist": artist,
            "albumId": album_id,
            "size": 5,
            "libraryId": library_id,
            "libraryPath": library_path,
            "missing": missing,
            "updatedAt": mtime(),
            "suffix": "flac",
            "bitRate": 912,
            "trackNumber": 1,
            "year": 2020,
            "duration": 200.0,
            "mbzRecordingID": mbid,
        })
    }

    #[tokio::test]
    async fn audit_detects_all_six_issue_categories_and_ignores_hidden_and_discovery() {
        let lib = Library::new().await;
        // Add a second folder for alice so we can test split albums across folders.
        lib.folder("alice", "alice-shared", "music/shared", true)
            .await;

        let music_root = lib.root.join("music");
        let music_root_str = music_root.to_string_lossy().into_owned();

        // 1. Clean track in alice-music/CleanAlbum with cover.jpg, .lrc, and MBID.
        lib.write("alice/CleanAlbum/01 - Clean.flac", b"audio");
        lib.write("alice/CleanAlbum/01 - Clean.lrc", b"[00:01.00] hello");
        lib.write("alice/CleanAlbum/cover.jpg", b"jpg");

        // 2. Track with no MBID, no cover art, no lyrics in alice-music/NoExtras.
        lib.write("alice/NoExtras/01 - Bare.flac", b"audio");

        // 3. Untracked audio file on disk (not in Navidrome).
        lib.write("alice/NoExtras/02 - Untracked.flac", b"audio");

        // 4. Hidden dir & Discovery dir files — must be skipped.
        lib.write("alice/.hidden/ignored.flac", b"audio");
        lib.write("alice/Discovery/Balanced/staged.flac", b"audio");

        // 5. Split album: track 1 in alice-music, track 2 in alice-shared.
        lib.write("alice/SplitAlbum/01 - Part1.flac", b"audio");
        lib.write("alice/SplitAlbum/cover.jpg", b"jpg");
        lib.write("alice/SplitAlbum/01 - Part1.lrc", b"lyrics");
        lib.write("shared/SplitAlbum/02 - Part2.flac", b"audio");
        lib.write("shared/SplitAlbum/cover.jpg", b"jpg");
        lib.write("shared/SplitAlbum/02 - Part2.lrc", b"lyrics");

        // 6. Missing on disk: Navidrome lists alice/Gone/01 - Missing.flac, which does not exist.
        // Also remove the default Library::new() track alice/A/x.flac so it doesn't pollute untracked.
        let _ = std::fs::remove_file(music_root.join("alice/A/x.flac"));

        let songs = json!([
            song_json(
                "s-clean",
                "alice/CleanAlbum/01 - Clean.flac",
                "Clean",
                "Artist A",
                "Clean Album",
                "al-clean",
                1,
                &music_root_str,
                Some("11111111-1111-4111-8111-111111111111"),
                false,
            ),
            song_json(
                "s-bare",
                "alice/NoExtras/01 - Bare.flac",
                "Bare",
                "Artist B",
                "No Extras",
                "al-bare",
                1,
                &music_root_str,
                None,
                false,
            ),
            song_json(
                "s-split-1",
                "alice/SplitAlbum/01 - Part1.flac",
                "Part 1",
                "Artist C",
                "Split Album",
                "al-split",
                1,
                &music_root_str,
                Some("22222222-2222-4222-8222-222222222222"),
                false,
            ),
            song_json(
                "s-split-2",
                "shared/SplitAlbum/02 - Part2.flac",
                "Part 2",
                "Artist C",
                "Split Album",
                "al-split",
                1,
                &music_root_str,
                Some("33333333-3333-4333-8333-333333333333"),
                false,
            ),
            song_json(
                "s-missing",
                "alice/Gone/01 - Missing.flac",
                "Missing",
                "Artist D",
                "Gone Album",
                "al-gone",
                1,
                &music_root_str,
                Some("44444444-4444-4444-8444-444444444444"),
                false,
            ),
        ]);

        let stub = NavidromeStub::start();
        let body = songs.to_string();
        stub.answer("/api/song", &[&body]);
        stub.header("/api/song", "X-Total-Count", "5");

        let report = audit_library(
            &lib.db,
            &stub.client(),
            None,
            &test_ctx("alice"),
            None,
            25,
        )
        .await
        .unwrap();

        assert_eq!(report.folders_audited.len(), 2);
        assert_eq!(report.summary.total_songs_in_navidrome, 5);
        assert_eq!(report.summary.total_audio_files_on_disk, 5);
        assert!(!report.truncated);

        // 1. untracked_on_disk
        assert_eq!(report.summary.untracked_on_disk, 1);
        assert!(report.untracked_on_disk[0]
            .path
            .ends_with("02 - Untracked.flac"));
        assert_eq!(report.untracked_on_disk[0].folder_id, "alice-music");

        // 2. missing_in_navidrome
        assert_eq!(report.summary.missing_in_navidrome, 1);
        assert_eq!(report.missing_in_navidrome[0].song_id, "s-missing");
        assert_eq!(
            report.missing_in_navidrome[0].folder_id.as_deref(),
            Some("alice-music")
        );

        // 3. split_albums
        assert_eq!(report.summary.split_albums, 1);
        assert_eq!(report.split_albums[0].album, "Split Album");
        assert_eq!(
            report.split_albums[0].folders,
            vec!["alice-music".to_string(), "alice-shared".to_string()]
        );
        assert_eq!(report.split_albums[0].track_count, 2);

        // 4. missing_mbids
        assert_eq!(report.summary.missing_mbids, 1);
        assert_eq!(report.missing_mbids[0].song_id, "s-bare");

        // 5. missing_cover_art
        assert_eq!(report.summary.missing_cover_art, 1);
        assert_eq!(report.missing_cover_art[0].album, "No Extras");
        assert_eq!(
            report.missing_cover_art[0].album_id.as_deref(),
            Some("al-bare")
        );
        assert_eq!(report.missing_cover_art[0].track_count, 2);

        // 6. missing_lyrics
        assert_eq!(report.summary.missing_lyrics, 1);
        assert_eq!(report.missing_lyrics[0].song_id, "s-bare");
    }

    #[tokio::test]
    async fn audit_refuses_another_users_folder_with_unknown_handle() {
        let lib = Library::new().await;
        let stub = NavidromeStub::start();
        stub.answer("/api/song", &["[]"]);
        stub.header("/api/song", "X-Total-Count", "0");

        let err = audit_library(
            &lib.db,
            &stub.client(),
            None,
            &test_ctx("alice"),
            Some("bob-music"),
            25,
        )
        .await
        .unwrap_err();

        assert_eq!(err.code, FailureCode::UnknownHandle);
    }

    #[tokio::test]
    async fn audit_caps_samples_and_reports_omitted_counts() {
        let lib = Library::new().await;
        let music_root = lib.root.join("music");
        let _ = std::fs::remove_file(music_root.join("alice/A/x.flac"));
        lib.write("alice/Dir/01.flac", b"audio");
        lib.write("alice/Dir/02.flac", b"audio");
        lib.write("alice/Dir/03.flac", b"audio");

        let stub = NavidromeStub::start();
        stub.answer("/api/song", &["[]"]);
        stub.header("/api/song", "X-Total-Count", "0");

        let report = audit_library(
            &lib.db,
            &stub.client(),
            None,
            &test_ctx("alice"),
            Some("alice-music"),
            1,
        )
        .await
        .unwrap();

        assert_eq!(report.summary.untracked_on_disk, 3);
        assert_eq!(report.untracked_on_disk.len(), 1);
        assert_eq!(report.omitted.untracked_on_disk, 2);
        assert!(report.truncated);
    }
}
