//! Source path validation and unimported audio file discovery for existing libraries.

use crate::auth::UserCtx;
use crate::core::database_failure;
use crate::folder_policy::{canonical_folder_path, overlap, Overlap, PolicyFolder, PolicyRows};
use crate::models::folder::Folder;
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::library_import::{
    LibraryImportBatch, LibraryImportItem, LibraryScanPreview, LibraryScanUnitPreview,
};
use soulbeet::audio::is_audio_path;
use sqlx::SqlitePool;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

/// Maximum directory nesting depth walked during a library scan.
const MAX_WALK_DEPTH: usize = 16;

/// Number of sample units included in a `LibraryScanPreview`.
const PREVIEW_SAMPLE_LIMIT: usize = 50;

/// One album directory or loose singleton track found during a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedUnit {
    pub source_path: PathBuf,
    pub display_name: String,
    pub is_album_dir: bool,
    pub file_count: u64,
    pub audio_files: Vec<PathBuf>,
}

/// Complete result of scanning a source folder against indexed beets paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanResult {
    pub source_path: PathBuf,
    pub target_path: PathBuf,
    pub in_place: bool,
    pub units: Vec<ScannedUnit>,
    pub album_units: u64,
    pub singleton_units: u64,
    pub total_files: u64,
    pub already_indexed_files: u64,
}

impl ScanResult {
    pub fn to_preview(&self) -> LibraryScanPreview {
        LibraryScanPreview {
            source_path: self.source_path.to_string_lossy().into_owned(),
            target_path: self.target_path.to_string_lossy().into_owned(),
            in_place: self.in_place,
            total_units: self.units.len() as u64,
            album_units: self.album_units,
            singleton_units: self.singleton_units,
            total_files: self.total_files,
            already_indexed_files: self.already_indexed_files,
            sample_units: self
                .units
                .iter()
                .take(PREVIEW_SAMPLE_LIMIT)
                .map(|u| LibraryScanUnitPreview {
                    source_path: u.source_path.to_string_lossy().into_owned(),
                    display_name: u.display_name.clone(),
                    is_album_dir: u.is_album_dir,
                    file_count: u.file_count,
                })
                .collect(),
        }
    }
}

fn invalid_input(message: impl Into<String>) -> Failure {
    Failure::new(
        FailureCode::InvalidInput,
        Stage::Input,
        RetryAdvice::NotRetryable,
        message,
    )
}

fn permission_denied(message: impl Into<String>) -> Failure {
    Failure::new(
        FailureCode::PermissionNotGranted,
        Stage::Input,
        RetryAdvice::NeedsUser,
        message,
    )
}

/// Validates `source_path` against `target_folder`, approved roots, and other users' libraries.
/// Returns `(canonical_source, canonical_target, in_place, exclude_dirs)`.
pub async fn resolve_and_validate_source_path_in(
    db: &SqlitePool,
    actor: &UserCtx,
    target_folder: &Folder,
    source_path: Option<&str>,
) -> Result<(PathBuf, PathBuf, bool, Vec<PathBuf>), Failure> {
    let target_checked = canonical_folder_path(Path::new(&target_folder.path))
        .map_err(|e| invalid_input(format!("Library folder path is invalid: {e}")))?;
    let target_canon = tokio::fs::canonicalize(&target_checked).await.map_err(|e| {
        invalid_input(format!(
            "Library folder {} does not exist or cannot be read: {e}",
            target_checked.display()
        ))
    })?;
    if !target_canon.is_dir() {
        return Err(invalid_input(format!(
            "Library folder {} is not a directory",
            target_canon.display()
        )));
    }

    let rows = PolicyRows::load_in(db)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    let (policy_folders, policy_roots) = rows.inputs();

    validate_source_with_policy(
        actor,
        &target_canon,
        source_path,
        &policy_folders,
        &policy_roots,
    )
    .await
}

/// Pure policy validation helper (split out for unit testing without needing DB rows).
pub(crate) async fn validate_source_with_policy(
    actor: &UserCtx,
    target_canon: &Path,
    source_path: Option<&str>,
    policy_folders: &[PolicyFolder],
    policy_roots: &[PathBuf],
) -> Result<(PathBuf, PathBuf, bool, Vec<PathBuf>), Failure> {
    let mut other_user_dirs: Vec<PathBuf> = Vec::new();
    for folder in policy_folders {
        if folder.user_id == actor.user_id {
            continue;
        }
        match &folder.path {
            Ok(path) => {
                if let Ok(real) = tokio::fs::canonicalize(path).await {
                    other_user_dirs.push(real);
                } else {
                    other_user_dirs.push(path.clone());
                }
            }
            Err(_) if !actor.role.is_admin() => {
                return Err(permission_denied(
                    "Another user's folder cannot be resolved, so external source paths cannot be checked",
                ));
            }
            Err(_) => {}
        }
    }

    let trimmed = source_path.map(str::trim).filter(|s| !s.is_empty());
    let Some(raw) = trimmed else {
        return Ok((
            target_canon.to_path_buf(),
            target_canon.to_path_buf(),
            true,
            other_user_dirs,
        ));
    };

    let raw_path = Path::new(raw);
    if !raw_path.is_absolute() {
        return Err(invalid_input("Source path must be an absolute path"));
    }
    if raw_path.components().any(|c| c == Component::ParentDir) {
        return Err(invalid_input("Source path must not contain '..'"));
    }
    let source_canon = tokio::fs::canonicalize(raw_path).await.map_err(|e| {
        invalid_input(format!(
            "Source folder {} does not exist or cannot be read: {e}",
            raw_path.display()
        ))
    })?;
    let meta = tokio::fs::metadata(&source_canon).await.map_err(|e| {
        invalid_input(format!(
            "Could not inspect source folder {}: {e}",
            source_canon.display()
        ))
    })?;
    if !meta.is_dir() {
        return Err(invalid_input(format!(
            "Source path {} is not a directory",
            source_canon.display()
        )));
    }

    // Check whether source is the user's own library folder or a subdirectory of it.
    if source_canon == target_canon || source_canon.starts_with(target_canon) {
        if let Ok(rel) = source_canon.strip_prefix(target_canon) {
            for (idx, comp) in rel.components().enumerate() {
                if let Component::Normal(name) = comp {
                    let s = name.to_string_lossy();
                    if s.starts_with('.') {
                        return Err(invalid_input(
                            "Hidden dot-directories cannot be scanned for library import",
                        ));
                    }
                    if idx == 0 && s == "Discovery" {
                        return Err(invalid_input(
                            "The Discovery staging folder cannot be scanned for library import",
                        ));
                    }
                }
            }
        }
        if !actor.role.is_admin() {
            for other in &other_user_dirs {
                if overlap(&source_canon, other).is_some() {
                    return Err(permission_denied(
                        "Source folder overlaps another user's private library folder",
                    ));
                }
            }
        }
        return Ok((
            source_canon,
            target_canon.to_path_buf(),
            true,
            other_user_dirs,
        ));
    }

    // External source folder (not inside target_canon).
    let matched_root = policy_roots
        .iter()
        .filter(|root| source_canon != **root && source_canon.starts_with(root))
        .max_by_key(|root| root.components().count());

    if let Some(root) = matched_root {
        if let Ok(rel) = source_canon.strip_prefix(root) {
            for comp in rel.components() {
                if let Component::Normal(name) = comp {
                    if name.to_string_lossy().starts_with('.') {
                        return Err(invalid_input(
                            "Hidden dot-directories cannot be scanned for library import",
                        ));
                    }
                }
            }
        }
    }

    if !actor.role.is_admin() {
        if matched_root.is_none() {
            return Err(permission_denied(
                "Source folder must be strictly inside an admin-approved folder root",
            ));
        }
        for other in &other_user_dirs {
            if let Some(kind) = overlap(&source_canon, other) {
                let msg = match kind {
                    Overlap::Equal => {
                        "Source folder is another user's private library folder"
                    }
                    Overlap::Contains => {
                        "Source folder contains another user's private library folder"
                    }
                    Overlap::Inside => {
                        "Source folder is inside another user's private library folder"
                    }
                };
                return Err(permission_denied(msg));
            }
        }
    }

    Ok((
        source_canon,
        target_canon.to_path_buf(),
        false,
        other_user_dirs,
    ))
}

fn is_within_roots(path: &Path, source_canon: Option<&Path>, target_canon: Option<&Path>) -> bool {
    source_canon.is_some_and(|root| path == root || path.starts_with(root))
        || target_canon.is_some_and(|root| path == root || path.starts_with(root))
}

/// Checks that no path component from `root` down to `path` is a symlink.
async fn has_symlink_relative_to(path: &Path, root: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    let mut cur = root.to_path_buf();
    for comp in rel.components() {
        if let Component::Normal(part) = comp {
            cur.push(part);
            match tokio::fs::symlink_metadata(&cur).await {
                Ok(meta) if meta.file_type().is_symlink() => return true,
                Ok(_) => {}
                Err(_) => return true,
            }
        }
    }
    false
}

/// Validates a library import unit right before invoking beets or writing tags:
/// - Refuses symlinks at the unit path or any component relative to the batch root.
/// - Verifies the canonical path is strictly contained within `batch.source_path` or
///   `batch.target_path`.
/// - For album directories, walks all direct and nested entries with `symlink_metadata`
///   and refuses if any directory or audio-extension entry (or escaping symlink) is a symlink.
///
/// Returns the canonicalized path of the validated unit.
pub(crate) async fn validate_unit_before_beets(
    batch: &LibraryImportBatch,
    item: &LibraryImportItem,
) -> Result<PathBuf, Failure> {
    let item_path = Path::new(&item.source_path);
    let meta = tokio::fs::symlink_metadata(item_path).await.map_err(|e| {
        Failure::new(
            FailureCode::ImportFilesGone,
            Stage::Import,
            RetryAdvice::NotRetryable,
            format!("Import source path does not exist or cannot be inspected: {e}"),
        )
    })?;
    if meta.file_type().is_symlink() {
        return Err(invalid_input("Refused to import symlinked path"));
    }
    if item.is_album_dir {
        if !meta.file_type().is_dir() {
            return Err(invalid_input(
                "Refused to import album unit: path is not a directory",
            ));
        }
    } else if !meta.file_type().is_file() {
        return Err(invalid_input(
            "Refused to import file unit: path is not a regular file",
        ));
    }

    let item_canon = tokio::fs::canonicalize(item_path).await.map_err(|e| {
        Failure::new(
            FailureCode::ImportFilesGone,
            Stage::Import,
            RetryAdvice::NotRetryable,
            format!("Could not resolve import source path: {e}"),
        )
    })?;
    let source_canon = tokio::fs::canonicalize(&batch.source_path).await.ok();
    let target_canon = tokio::fs::canonicalize(&batch.target_path).await.ok();

    if !is_within_roots(&item_canon, source_canon.as_deref(), target_canon.as_deref()) {
        return Err(invalid_input(
            "Refused to import path: unit escapes the batch source and library directories",
        ));
    }

    // Refuse symlinked intermediate directories between the batch root and item_path.
    if let Some(root) = source_canon.as_deref() {
        if item_path.starts_with(root) && has_symlink_relative_to(item_path, root).await {
            return Err(invalid_input(
                "Refused to import path through a symlinked directory",
            ));
        }
    }
    if let Some(root) = target_canon.as_deref() {
        if item_path.starts_with(root) && has_symlink_relative_to(item_path, root).await {
            return Err(invalid_input(
                "Refused to import path through a symlinked directory",
            ));
        }
    }

    if item.is_album_dir {
        let mut stack: Vec<(PathBuf, usize)> = vec![(item_canon.clone(), 0)];
        while let Some((dir, depth)) = stack.pop() {
            if depth > MAX_WALK_DEPTH {
                continue;
            }
            let mut entries = tokio::fs::read_dir(&dir).await.map_err(|e| {
                Failure::new(
                    FailureCode::ImportFilesGone,
                    Stage::Import,
                    RetryAdvice::NotRetryable,
                    format!("Could not inspect album directory {}: {e}", dir.display()),
                )
            })?;
            while let Some(entry) = entries.next_entry().await.map_err(|e| {
                Failure::new(
                    FailureCode::ImportFilesGone,
                    Stage::Import,
                    RetryAdvice::NotRetryable,
                    format!("Could not read entry in album directory {}: {e}", dir.display()),
                )
            })? {
                let entry_path = entry.path();
                let entry_meta = tokio::fs::symlink_metadata(&entry_path).await.map_err(|e| {
                    Failure::new(
                        FailureCode::ImportFilesGone,
                        Stage::Import,
                        RetryAdvice::NotRetryable,
                        format!("Could not inspect entry {}: {e}", entry_path.display()),
                    )
                })?;
                let ft = entry_meta.file_type();
                if ft.is_symlink() {
                    let target_is_dir = tokio::fs::metadata(&entry_path)
                        .await
                        .is_ok_and(|m| m.is_dir());
                    let target_is_audio = tokio::fs::read_link(&entry_path)
                        .await
                        .is_ok_and(|t| is_audio_path(&t));
                    let target_escapes = tokio::fs::canonicalize(&entry_path)
                        .await
                        .is_ok_and(|c| {
                            !is_within_roots(&c, source_canon.as_deref(), target_canon.as_deref())
                        });
                    if is_audio_path(&entry_path)
                        || target_is_dir
                        || target_is_audio
                        || target_escapes
                    {
                        return Err(invalid_input(format!(
                            "Refused to import album directory containing symlink: {}",
                            entry_path.display()
                        )));
                    }
                } else if ft.is_dir() && depth < MAX_WALK_DEPTH {
                    stack.push((entry_path, depth + 1));
                }
            }
        }
    }

    Ok(item_canon)
}

fn is_excluded_dir(dir: &Path, exclude_dirs: &[PathBuf]) -> bool {
    exclude_dirs
        .iter()
        .any(|ex| dir == ex || dir.starts_with(ex))
}

/// Walks `source_root` (up to depth 16, never following symlinks), excluding hidden
/// dot-directories, `target_root/Discovery`, and `exclude_dirs`, and groups unindexed
/// audio files into album-directory units and root-level singleton units.
pub async fn scan_unimported_units(
    source_root: &Path,
    target_root: &Path,
    indexed: &HashSet<PathBuf>,
    exclude_dirs: &[PathBuf],
) -> Result<ScanResult, Failure> {
    let source_canon = tokio::fs::canonicalize(source_root)
        .await
        .unwrap_or_else(|_| source_root.to_path_buf());
    let target_canon = tokio::fs::canonicalize(target_root)
        .await
        .unwrap_or_else(|_| target_root.to_path_buf());
    let discovery_dir = target_canon.join("Discovery");
    let in_place = source_canon == target_canon || source_canon.starts_with(&target_canon);

    let mut already_indexed_files: u64 = 0;
    let mut unindexed_by_parent: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    let mut indexed_count_by_parent: HashMap<PathBuf, u64> = HashMap::new();
    let mut dirs_with_audio: HashSet<PathBuf> = HashSet::new();

    if !is_excluded_dir(&source_canon, exclude_dirs) && source_canon != discovery_dir {
        let mut stack: Vec<(PathBuf, usize)> = vec![(source_canon.clone(), 0)];
        while let Some((dir, depth)) = stack.pop() {
            if depth > MAX_WALK_DEPTH {
                continue;
            }
            let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
                continue;
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                let Ok(file_type) = entry.file_type().await else {
                    continue;
                };
                if file_type.is_symlink() {
                    continue;
                }
                if file_type.is_dir() {
                    let is_hidden = entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with('.');
                    if is_hidden
                        || path == discovery_dir
                        || is_excluded_dir(&path, exclude_dirs)
                    {
                        continue;
                    }
                    if depth < MAX_WALK_DEPTH {
                        stack.push((path, depth + 1));
                    }
                } else if file_type.is_file() && is_audio_path(&path) {
                    dirs_with_audio.insert(dir.clone());
                    let is_indexed = indexed.contains(&path)
                        || tokio::fs::canonicalize(&path)
                            .await
                            .is_ok_and(|c| indexed.contains(&c));
                    if is_indexed {
                        already_indexed_files += 1;
                        *indexed_count_by_parent.entry(dir.clone()).or_insert(0) += 1;
                    } else {
                        unindexed_by_parent
                            .entry(dir.clone())
                            .or_default()
                            .push(path);
                    }
                }
            }
        }
    }

    // Track every directory that has at least one child subdirectory containing audio files.
    let mut dirs_with_audio_descendant: HashSet<PathBuf> = HashSet::new();
    for audio_dir in &dirs_with_audio {
        let mut cur = audio_dir.parent();
        while let Some(ancestor) = cur {
            if ancestor == source_canon || !ancestor.starts_with(&source_canon) {
                break;
            }
            dirs_with_audio_descendant.insert(ancestor.to_path_buf());
            cur = ancestor.parent();
        }
    }

    let mut units: Vec<ScannedUnit> = Vec::new();
    let mut album_units: u64 = 0;
    let mut singleton_units: u64 = 0;
    let mut total_files: u64 = 0;

    for (parent_dir, mut files) in unindexed_by_parent {
        files.sort();
        let is_root = parent_dir == source_canon;
        let is_partially_indexed = indexed_count_by_parent.get(&parent_dir).copied().unwrap_or(0) > 0;
        let has_nested_audio_dirs = dirs_with_audio_descendant.contains(&parent_dir);

        if is_root || is_partially_indexed || has_nested_audio_dirs {
            for file in files {
                let display_name = file
                    .strip_prefix(&source_canon)
                    .map(|rel| rel.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| {
                        file.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| file.to_string_lossy().into_owned())
                    });
                units.push(ScannedUnit {
                    source_path: file.clone(),
                    display_name,
                    is_album_dir: false,
                    file_count: 1,
                    audio_files: vec![file],
                });
                singleton_units += 1;
                total_files += 1;
            }
        } else {
            let display_name = parent_dir
                .strip_prefix(&source_canon)
                .map(|rel| rel.to_string_lossy().into_owned())
                .unwrap_or_else(|_| parent_dir.to_string_lossy().into_owned());
            let count = files.len() as u64;
            units.push(ScannedUnit {
                source_path: parent_dir,
                display_name,
                is_album_dir: true,
                file_count: count,
                audio_files: files,
            });
            album_units += 1;
            total_files += count;
        }
    }

    units.sort_by(|a, b| {
        a.display_name
            .cmp(&b.display_name)
            .then_with(|| a.source_path.cmp(&b.source_path))
    });

    Ok(ScanResult {
        source_path: source_canon,
        target_path: target_canon,
        in_place,
        units,
        album_units,
        singleton_units,
        total_files,
        already_indexed_files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Role;

    fn ctx(user: &str, role: Role) -> UserCtx {
        UserCtx {
            user_id: user.to_string(),
            username: user.to_string(),
            role,
        }
    }

    fn policy_folder(user: &str, path: &Path) -> PolicyFolder {
        PolicyFolder {
            id: format!("f-{user}"),
            user_id: user.to_string(),
            owner_is_admin: false,
            path: Ok(path.to_path_buf()),
            stored_path: path.to_path_buf(),
        }
    }

    #[tokio::test]
    async fn scan_groups_album_dirs_and_loose_singletons_and_skips_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let target = root.join("music/alice");
        let other = target.join("OtherUser");
        std::fs::create_dir_all(target.join("Artist/Album1")).unwrap();
        std::fs::create_dir_all(target.join("Discovery/Balanced")).unwrap();
        std::fs::create_dir_all(target.join(".hidden")).unwrap();
        std::fs::create_dir_all(&other).unwrap();

        // Loose singleton at root
        std::fs::write(target.join("loose.flac"), b"a").unwrap();
        // Clean leaf album dir with 2 unindexed + 1 non-audio
        std::fs::write(target.join("Artist/Album1/01.flac"), b"a").unwrap();
        std::fs::write(target.join("Artist/Album1/02.mp3"), b"a").unwrap();
        std::fs::write(target.join("Artist/Album1/cover.jpg"), b"img").unwrap();
        // Partially-indexed album dir: 1 already-indexed + 1 unindexed -> emits singleton
        std::fs::create_dir_all(target.join("Artist/PartialAlbum")).unwrap();
        let indexed_file = target.join("Artist/PartialAlbum/01.flac");
        std::fs::write(&indexed_file, b"a").unwrap();
        std::fs::write(target.join("Artist/PartialAlbum/02.flac"), b"a").unwrap();
        // Parent dir with loose file + nested audio subdir -> parent file is singleton, leaf subdir is album
        std::fs::create_dir_all(target.join("Artist/Nested/CD1")).unwrap();
        std::fs::write(target.join("Artist/Nested/bonus.flac"), b"a").unwrap();
        std::fs::write(target.join("Artist/Nested/CD1/01.flac"), b"a").unwrap();
        // Discovery, hidden, excluded dir files (must all be skipped)
        std::fs::write(target.join("Discovery/Balanced/disc.flac"), b"a").unwrap();
        std::fs::write(target.join(".hidden/secret.flac"), b"a").unwrap();
        std::fs::write(other.join("bob.flac"), b"a").unwrap();

        let mut indexed = HashSet::new();
        indexed.insert(indexed_file);

        let res = scan_unimported_units(&target, &target, &indexed, &[other])
            .await
            .unwrap();

        assert!(res.in_place);
        assert_eq!(res.already_indexed_files, 1);
        assert_eq!(res.album_units, 2);
        assert_eq!(res.singleton_units, 3);
        assert_eq!(res.total_files, 6);
        assert_eq!(res.units.len(), 5);

        let by_name: HashMap<&str, &ScannedUnit> = res
            .units
            .iter()
            .map(|u| (u.display_name.as_str(), u))
            .collect();
        assert!(by_name["Artist/Album1"].is_album_dir);
        assert_eq!(by_name["Artist/Album1"].file_count, 2);
        assert!(by_name["Artist/Nested/CD1"].is_album_dir);
        assert_eq!(by_name["Artist/Nested/CD1"].file_count, 1);
        assert!(!by_name["Artist/Nested/bonus.flac"].is_album_dir);
        assert_eq!(by_name["Artist/Nested/bonus.flac"].file_count, 1);
        assert!(!by_name["Artist/PartialAlbum/02.flac"].is_album_dir);
        assert_eq!(by_name["Artist/PartialAlbum/02.flac"].file_count, 1);
        assert!(!by_name["loose.flac"].is_album_dir);
        assert_eq!(by_name["loose.flac"].file_count, 1);
    }

    #[tokio::test]
    async fn validate_source_enforces_roots_and_other_user_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let approved = root.join("music");
        let alice_lib = approved.join("alice");
        let bob_lib = approved.join("bob");
        let shared_drop = approved.join("incoming");
        let external_hidden = approved.join(".beets");
        let external_nested_hidden = approved.join("incoming/.Trash");
        let outside = root.join("outside");
        for p in [
            &alice_lib,
            &bob_lib,
            &shared_drop,
            &external_hidden,
            &external_nested_hidden,
            &outside,
        ] {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::create_dir_all(alice_lib.join("Discovery")).unwrap();
        std::fs::create_dir_all(alice_lib.join(".hidden")).unwrap();

        let folders = vec![
            policy_folder("alice", &alice_lib),
            policy_folder("bob", &bob_lib),
        ];
        let roots = vec![approved.clone()];
        let alice = ctx("alice", Role::User);
        let admin = ctx("admin", Role::Admin);

        // Non-admin: own library allowed (in_place = true)
        let (_, _, in_place, _) =
            validate_source_with_policy(&alice, &alice_lib, None, &folders, &roots)
                .await
                .unwrap();
        assert!(in_place);

        // Non-admin: Discovery and hidden dot-dir inside own library refused
        assert_eq!(
            validate_source_with_policy(
                &alice,
                &alice_lib,
                Some(alice_lib.join("Discovery").to_str().unwrap()),
                &folders,
                &roots,
            )
            .await
            .unwrap_err()
            .code,
            FailureCode::InvalidInput
        );
        assert_eq!(
            validate_source_with_policy(
                &alice,
                &alice_lib,
                Some(alice_lib.join(".hidden").to_str().unwrap()),
                &folders,
                &roots,
            )
            .await
            .unwrap_err()
            .code,
            FailureCode::InvalidInput
        );

        // External hidden dot-directories inside approved root refused (both user and admin)
        for hidden_path in [&external_hidden, &external_nested_hidden] {
            assert_eq!(
                validate_source_with_policy(
                    &alice,
                    &alice_lib,
                    Some(hidden_path.to_str().unwrap()),
                    &folders,
                    &roots,
                )
                .await
                .unwrap_err()
                .code,
                FailureCode::InvalidInput
            );
            assert_eq!(
                validate_source_with_policy(
                    &admin,
                    &alice_lib,
                    Some(hidden_path.to_str().unwrap()),
                    &folders,
                    &roots,
                )
                .await
                .unwrap_err()
                .code,
                FailureCode::InvalidInput
            );
        }

        // Non-admin: external folder strictly inside approved root & non-overlapping allowed
        let (_, _, in_place, _) = validate_source_with_policy(
            &alice,
            &alice_lib,
            Some(shared_drop.to_str().unwrap()),
            &folders,
            &roots,
        )
        .await
        .unwrap();
        assert!(!in_place);

        // Non-admin: root itself refused (contains other users AND equals root)
        assert_eq!(
            validate_source_with_policy(
                &alice,
                &alice_lib,
                Some(approved.to_str().unwrap()),
                &folders,
                &roots,
            )
            .await
            .unwrap_err()
            .code,
            FailureCode::PermissionNotGranted
        );

        // Non-admin: another user's library refused
        assert_eq!(
            validate_source_with_policy(
                &alice,
                &alice_lib,
                Some(bob_lib.to_str().unwrap()),
                &folders,
                &roots,
            )
            .await
            .unwrap_err()
            .code,
            FailureCode::PermissionNotGranted
        );

        // Non-admin: outside approved root refused
        assert_eq!(
            validate_source_with_policy(
                &alice,
                &alice_lib,
                Some(outside.to_str().unwrap()),
                &folders,
                &roots,
            )
            .await
            .unwrap_err()
            .code,
            FailureCode::PermissionNotGranted
        );

        // Admin: outside approved root allowed, and exclude_dirs contains other users' libraries
        let (_, _, in_place, excludes) = validate_source_with_policy(
            &admin,
            &alice_lib,
            Some(outside.to_str().unwrap()),
            &folders,
            &roots,
        )
        .await
        .unwrap();
        assert!(!in_place);
        assert!(excludes.contains(&bob_lib));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn validate_unit_before_beets_rejects_symlinks_inside_album_dir_and_escapes() {
        use shared::library_import::{
            LibraryImportBatchStatus, LibraryImportItemStatus, LibraryImportMode,
            LibraryImportOrigin,
        };

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let alice_lib = root.join("music/alice");
        let outside = root.join("outside");
        let album_with_audio_link = alice_lib.join("Artist/AlbumAudioLink");
        let album_with_dir_link = alice_lib.join("Artist/AlbumDirLink");
        let clean_album = alice_lib.join("Artist/CleanAlbum");
        for p in [
            &alice_lib,
            &outside,
            &album_with_audio_link,
            &album_with_dir_link,
            &clean_album,
        ] {
            std::fs::create_dir_all(p).unwrap();
        }
        let secret = outside.join("secret.flac");
        std::fs::write(&secret, b"secret").unwrap();
        std::fs::write(album_with_audio_link.join("01.flac"), b"ok").unwrap();
        std::os::unix::fs::symlink(&secret, album_with_audio_link.join("02.flac")).unwrap();
        std::os::unix::fs::symlink(&outside, album_with_dir_link.join("sub")).unwrap();
        std::fs::write(clean_album.join("01.flac"), b"ok").unwrap();

        let batch = LibraryImportBatch {
            id: "b1".into(),
            user_id: "alice".into(),
            target_folder_id: "f1".into(),
            target_path: alice_lib.to_string_lossy().into_owned(),
            source_path: alice_lib.to_string_lossy().into_owned(),
            mode: LibraryImportMode::TagInPlace,
            status: LibraryImportBatchStatus::Running,
            total_units: 1,
            total_files: 1,
            completed_units: 0,
            imported_units: 0,
            needs_decision_units: 0,
            skipped_units: 0,
            failed_units: 0,
            already_indexed_files: 0,
            origin: LibraryImportOrigin::Ui,
            error: None,
            created_at: String::new(),
            updated_at: String::new(),
            finished_at: None,
        };
        let make_item = |path: &Path, is_album_dir: bool| LibraryImportItem {
            id: "i1".into(),
            batch_id: "b1".into(),
            seq: 0,
            source_path: path.to_string_lossy().into_owned(),
            display_name: "unit".into(),
            is_album_dir,
            file_count: 1,
            status: LibraryImportItemStatus::Pending,
            skip_kind: None,
            failure: None,
            choices: None,
            updated_at: String::new(),
        };

        assert!(validate_unit_before_beets(&batch, &make_item(&clean_album, true))
            .await
            .is_ok());
        assert_eq!(
            validate_unit_before_beets(&batch, &make_item(&album_with_audio_link, true))
                .await
                .unwrap_err()
                .code,
            FailureCode::InvalidInput
        );
        assert_eq!(
            validate_unit_before_beets(&batch, &make_item(&album_with_dir_link, true))
                .await
                .unwrap_err()
                .code,
            FailureCode::InvalidInput
        );
        assert_eq!(
            validate_unit_before_beets(&batch, &make_item(&secret, false))
                .await
                .unwrap_err()
                .code,
            FailureCode::InvalidInput
        );
    }
}
