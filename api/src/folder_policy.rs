//! Where a folder may live: canonical paths, approved roots for non-admins,
//! and no overlap with another user's folder unless that folder is shared.

use crate::models::folder::Folder;
#[cfg(feature = "server")]
use crate::models::{folder::OwnedFolder, folder_root::FolderRoot};
use serde::{Deserialize, Serialize};

#[cfg(feature = "server")]
use crate::auth::UserCtx;
#[cfg(feature = "server")]
use sqlx::SqlitePool;
#[cfg(feature = "server")]
use std::fmt;
#[cfg(feature = "server")]
use std::path::{Component, Path, PathBuf};

/// How a folder breaks the rules. `F` names the other folder: an index into
/// the checked list on the server, a [`FolderLabel`] in the admin view.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ConflictKind<F> {
    SameAsOther(F),
    ContainsOther(F),
    InsideOther(F),
    OutsideApprovedRoots,
    Unresolvable { error: String },
}

#[cfg(feature = "server")]
impl<F> ConflictKind<F> {
    fn map<G>(self, f: impl FnOnce(F) -> G) -> ConflictKind<G> {
        match self {
            ConflictKind::SameAsOther(o) => ConflictKind::SameAsOther(f(o)),
            ConflictKind::ContainsOther(o) => ConflictKind::ContainsOther(f(o)),
            ConflictKind::InsideOther(o) => ConflictKind::InsideOther(f(o)),
            ConflictKind::OutsideApprovedRoots => ConflictKind::OutsideApprovedRoots,
            ConflictKind::Unresolvable { error } => ConflictKind::Unresolvable { error },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FolderConflict<F> {
    pub folder: F,
    pub kind: ConflictKind<F>,
}

/// A folder as the admin panel names it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FolderLabel {
    pub owner: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FolderAdminEntry {
    pub folder: Folder,
    pub owner: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FolderAdminView {
    pub roots: Vec<String>,
    pub folders: Vec<FolderAdminEntry>,
    pub conflicts: Vec<FolderConflict<FolderLabel>>,
}

#[cfg(feature = "server")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FolderPathError {
    NotAbsolute,
    ParentComponent,
    Unresolvable(String),
}

#[cfg(feature = "server")]
impl fmt::Display for FolderPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FolderPathError::NotAbsolute => f.write_str("Folder path must be absolute"),
            FolderPathError::ParentComponent => f.write_str("Folder path must not contain '..'"),
            FolderPathError::Unresolvable(e) => write!(f, "Cannot resolve the folder path: {e}"),
        }
    }
}

/// Resolves symlinks in the deepest part of `path` that exists and appends
/// the rest, so a folder that is not created yet still gets its real place.
#[cfg(feature = "server")]
pub fn canonical_folder_path(path: &Path) -> Result<PathBuf, FolderPathError> {
    if !path.is_absolute() {
        return Err(FolderPathError::NotAbsolute);
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(FolderPathError::ParentComponent);
    }
    let unresolvable = |e: std::io::Error| FolderPathError::Unresolvable(e.to_string());
    let mut existing = path;
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                existing = existing.parent().ok_or_else(|| unresolvable(e))?;
            }
            Err(e) => return Err(unresolvable(e)),
        }
    }
    let mut resolved = std::fs::canonicalize(existing).map_err(unresolvable)?;
    resolved.extend(
        path.strip_prefix(existing)
            .expect("an ancestor is a prefix of its path")
            .components(),
    );
    if resolved.to_str().is_none() {
        return Err(FolderPathError::Unresolvable(
            "the path is not valid UTF-8".to_string(),
        ));
    }
    Ok(resolved)
}

/// How `a` relates to `b`.
#[cfg(feature = "server")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overlap {
    Equal,
    Contains,
    Inside,
}

/// Compares by component, so `/music/bob` does not contain `/music/bobby`.
#[cfg(feature = "server")]
pub fn overlap(a: &Path, b: &Path) -> Option<Overlap> {
    if a == b {
        Some(Overlap::Equal)
    } else if b.starts_with(a) {
        Some(Overlap::Contains)
    } else if a.starts_with(b) {
        Some(Overlap::Inside)
    } else {
        None
    }
}

/// A stored folder with its path run through `canonical_folder_path`, or
/// the error text when that failed.
#[cfg(feature = "server")]
#[derive(Clone, Debug)]
pub struct PolicyFolder {
    pub id: String,
    pub user_id: String,
    pub owner_is_admin: bool,
    pub path: Result<PathBuf, String>,
    /// The path as stored, for a folder whose `path` did not resolve.
    pub stored_path: PathBuf,
    pub shared: bool,
}

#[cfg(feature = "server")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FolderRefusal {
    Path(FolderPathError),
    /// Not strictly inside one of `roots`, the approved roots as checked.
    OutsideApprovedRoots {
        roots: Vec<PathBuf>,
    },
    Overlaps(Overlap),
    Unchecked,
    Changed,
}

#[cfg(feature = "server")]
impl fmt::Display for FolderRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FolderRefusal::Path(e) => return e.fmt(f),
            // Only non-admins get this, and they cannot see the roots list.
            FolderRefusal::OutsideApprovedRoots { roots } if roots.is_empty() => {
                "No folder roots are approved yet, so you cannot add a folder. Ask an admin to approve one."
            }
            FolderRefusal::OutsideApprovedRoots { roots } => {
                let roots: Vec<_> = roots.iter().map(|r| r.display().to_string()).collect();
                return write!(
                    f,
                    "This path is not inside a folder root an admin approved. Use a path inside {}, or ask an admin to approve another root.",
                    roots.join(", ")
                );
            }
            FolderRefusal::Overlaps(Overlap::Equal) => {
                "Another user already has a folder at this path. An admin can mark that folder shared."
            }
            FolderRefusal::Overlaps(Overlap::Contains) => {
                "This path contains another user's folder. An admin can mark that folder shared."
            }
            FolderRefusal::Overlaps(Overlap::Inside) => {
                "This path is inside another user's folder. An admin can mark that folder shared."
            }
            FolderRefusal::Unchecked => {
                "Another user's folder cannot be resolved, so this path cannot be checked. An admin can mark that folder shared or delete it in Settings > Library."
            }
            FolderRefusal::Changed => "The folder path changed while it was being created",
        })
    }
}

/// A root approves the folders strictly inside it, not the root itself:
/// one user holding the whole root would block everyone else there.
#[cfg(feature = "server")]
fn inside_a_root(path: &Path, roots: &[PathBuf]) -> bool {
    roots
        .iter()
        .any(|root| path != root && path.starts_with(root))
}

/// Whether `actor` may keep a folder at the canonical path `candidate`.
#[cfg(feature = "server")]
pub fn check_folder_path(
    candidate: &Path,
    actor: &UserCtx,
    folders: &[PolicyFolder],
    roots: &[PathBuf],
) -> Result<(), FolderRefusal> {
    if !actor.role.is_admin() && !inside_a_root(candidate, roots) {
        return Err(FolderRefusal::OutsideApprovedRoots {
            roots: roots.to_vec(),
        });
    }
    // A shared folder may be overlapped, so it needs no resolved path.
    for other in folders
        .iter()
        .filter(|f| f.user_id != actor.user_id && !f.shared)
    {
        let path = other.path.as_ref().map_err(|_| FolderRefusal::Unchecked)?;
        if let Some(kind) = overlap(candidate, path) {
            return Err(FolderRefusal::Overlaps(kind));
        }
    }
    Ok(())
}

/// Whether `actor` may move their folder stored at `stored` to the canonical
/// path `candidate`. A folder that stays where it is passes as it stands, so
/// one that conflicts since the upgrade can still be renamed.
#[cfg(feature = "server")]
pub fn check_folder_update(
    candidate: &Path,
    stored: &str,
    actor: &UserCtx,
    folders: &[PolicyFolder],
    roots: &[PathBuf],
) -> Result<(), FolderRefusal> {
    if canonical_folder_path(Path::new(stored)).is_ok_and(|current| current == candidate) {
        return Ok(());
    }
    check_folder_path(candidate, actor, folders, roots)
}

/// Whose a file is, for removing it.
#[cfg(feature = "server")]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FileOwner<'a> {
    /// The deepest folder that holds the file is the actor's.
    Actor,
    /// The deepest folder that holds it is someone else's, folders of several
    /// owners tie, or no folder holds it.
    Other,
    /// Another user's folder, stored as `folder`, cannot be resolved, and
    /// comparing paths does not show that the file is outside it.
    Unresolved { folder: &'a Path, error: &'a str },
}

/// Who owns the canonical file `path`: the owner of the deepest stored
/// folder, of any user, that holds it. A file that sits in someone else's
/// folder, nested in or shared over the actor's, is theirs to remove, not
/// the actor's; when folders of several owners tie, it belongs to none
/// alone. Another user's folder that cannot be resolved may hold it, so it
/// is not the actor's unless the stored path shows it is outside that folder.
#[cfg(feature = "server")]
pub fn file_owner<'a>(path: &Path, folders: &'a [PolicyFolder], actor: &str) -> FileOwner<'a> {
    let unresolved = folders.iter().find_map(|f| match &f.path {
        Err(error) if f.user_id != actor && !lexically_outside(path, &f.stored_path) => {
            Some(FileOwner::Unresolved {
                folder: &f.stored_path,
                error,
            })
        }
        _ => None,
    });
    if let Some(owner) = unresolved {
        return owner;
    }
    let holding: Vec<(&PolicyFolder, usize)> = folders
        .iter()
        .filter_map(|f| {
            let folder = f.path.as_ref().ok()?;
            (path != folder && path.starts_with(folder)).then(|| (f, folder.components().count()))
        })
        .collect();
    let Some(deepest) = holding.iter().map(|(_, depth)| *depth).max() else {
        return FileOwner::Other;
    };
    if holding
        .iter()
        .filter(|(_, depth)| *depth == deepest)
        .all(|(f, _)| f.user_id == actor)
    {
        FileOwner::Actor
    } else {
        FileOwner::Other
    }
}

/// Whether the stored folder path `stored` shows, compared as written, that
/// `file` is not inside it. A relative path or one with `..` shows nothing.
#[cfg(feature = "server")]
fn lexically_outside(file: &Path, stored: &Path) -> bool {
    stored.is_absolute()
        && !stored.components().any(|c| c == Component::ParentDir)
        && !file.starts_with(stored)
}

/// Every stored folder that breaks the rules today, for admins to sort out.
/// Folders are named by their index in `folders`.
#[cfg(feature = "server")]
pub fn folder_conflicts(folders: &[PolicyFolder], roots: &[PathBuf]) -> Vec<FolderConflict<usize>> {
    let mut conflicts = Vec::new();
    for (i, folder) in folders.iter().enumerate() {
        let path = match &folder.path {
            Ok(path) => path,
            Err(error) => {
                conflicts.push(FolderConflict {
                    folder: i,
                    kind: ConflictKind::Unresolvable {
                        error: error.clone(),
                    },
                });
                continue;
            }
        };
        if !folder.owner_is_admin && !inside_a_root(path, roots) {
            conflicts.push(FolderConflict {
                folder: i,
                kind: ConflictKind::OutsideApprovedRoots,
            });
        }
        if folder.shared {
            continue;
        }
        for (j, other) in folders.iter().enumerate() {
            if other.user_id == folder.user_id || other.shared {
                continue;
            }
            let Ok(other_path) = &other.path else {
                continue;
            };
            let kind = match overlap(path, other_path) {
                Some(Overlap::Equal) => ConflictKind::SameAsOther(j),
                Some(Overlap::Contains) => ConflictKind::ContainsOther(j),
                Some(Overlap::Inside) => ConflictKind::InsideOther(j),
                None => continue,
            };
            conflicts.push(FolderConflict { folder: i, kind });
        }
    }
    conflicts
}

/// Canonicalizes every stored folder and root. The folder at index `i` is
/// `folders[i]`. A root that does not resolve is left out, so it approves
/// nothing.
#[cfg(feature = "server")]
pub(crate) fn policy_inputs(
    folders: &[OwnedFolder],
    roots: &[FolderRoot],
) -> (Vec<PolicyFolder>, Vec<PathBuf>) {
    let folders = folders
        .iter()
        .map(|f| PolicyFolder {
            id: f.folder.id.clone(),
            owner_is_admin: f.owner_role.is_admin(),
            path: canonical_folder_path(Path::new(&f.folder.path)).map_err(|e| e.to_string()),
            stored_path: PathBuf::from(&f.folder.path),
            user_id: f.folder.user_id.clone(),
            shared: f.folder.shared,
        })
        .collect();
    let roots = roots
        .iter()
        .filter_map(|r| canonical_folder_path(Path::new(&r.path)).ok())
        .collect();
    (folders, roots)
}

/// The stored rows the folder rules read, loaded once per decision.
#[cfg(feature = "server")]
pub struct PolicyRows {
    pub folders: Vec<OwnedFolder>,
    pub roots: Vec<FolderRoot>,
}

#[cfg(feature = "server")]
impl PolicyRows {
    pub async fn load() -> Result<PolicyRows, String> {
        Self::load_in(&crate::db::DB).await
    }

    pub(crate) async fn load_in(db: &SqlitePool) -> Result<PolicyRows, String> {
        Ok(PolicyRows {
            folders: Folder::get_all_with_owners_in(db).await?,
            roots: FolderRoot::get_all_in(db).await?,
        })
    }

    pub fn inputs(&self) -> (Vec<PolicyFolder>, Vec<PathBuf>) {
        policy_inputs(&self.folders, &self.roots)
    }

    /// The admin panel's view: roots as stored (so each can be removed as
    /// stored), every folder, and the conflicts with both folders named.
    pub fn admin_view(self) -> FolderAdminView {
        let (policy_folders, policy_roots) = self.inputs();
        let conflicts = folder_conflicts(&policy_folders, &policy_roots);
        let labels: Vec<FolderLabel> = self
            .folders
            .iter()
            .map(|f| FolderLabel {
                owner: f.owner.clone(),
                name: f.folder.name.clone(),
            })
            .collect();
        let label = |i: usize| labels[i].clone();
        FolderAdminView {
            roots: self.roots.into_iter().map(|r| r.path).collect(),
            conflicts: conflicts
                .into_iter()
                .map(|c| FolderConflict {
                    folder: label(c.folder),
                    kind: c.kind.map(label),
                })
                .collect(),
            folders: self
                .folders
                .into_iter()
                .map(|f| FolderAdminEntry {
                    folder: f.folder,
                    owner: f.owner,
                })
                .collect(),
        }
    }
}

/// Startup re-validation: SQL cannot canonicalize, so existing rows are
/// checked here on every start.
#[cfg(feature = "server")]
pub async fn log_folder_conflicts() {
    match PolicyRows::load().await {
        Ok(rows) => {
            let (folders, roots) = rows.inputs();
            let n = folder_conflicts(&folders, &roots).len();
            if n > 0 {
                tracing::warn!("{n} folder conflicts found after the upgrade checks; an admin can review them in Settings > Library");
            }
        }
        Err(e) => tracing::warn!("Could not check library folders: {e}"),
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::auth::Role;
    use std::os::unix::fs::symlink;

    fn pf(user: &str, admin: bool, path: &Path, shared: bool) -> PolicyFolder {
        PolicyFolder {
            id: format!("{user}:{}", path.display()),
            user_id: user.to_string(),
            owner_is_admin: admin,
            path: Ok(path.to_path_buf()),
            stored_path: path.to_path_buf(),
            shared,
        }
    }

    /// `pf` for a folder stored as `stored` that does not resolve.
    fn unresolved(user: &str, stored: &str) -> PolicyFolder {
        PolicyFolder {
            path: Err("Too many levels of symbolic links".to_string()),
            ..pf(user, false, Path::new(stored), false)
        }
    }

    fn actor(user: &str, role: Role) -> UserCtx {
        UserCtx {
            user_id: user.to_string(),
            username: user.to_string(),
            role,
        }
    }

    /// A tempdir whose own path is canonical, so expected values compare
    /// with what `canonical_folder_path` returns.
    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        (dir, root)
    }

    #[test]
    fn canonical_paths_resolve_symlinks_and_keep_missing_tails() {
        let (_dir, tmp) = tmp();
        std::fs::create_dir(tmp.join("real")).unwrap();
        symlink(tmp.join("real"), tmp.join("link")).unwrap();

        assert_eq!(
            canonical_folder_path(&tmp.join("link/new/dir")),
            Ok(tmp.join("real/new/dir"))
        );
        assert_eq!(
            canonical_folder_path(&tmp.join("link")),
            Ok(tmp.join("real"))
        );
        let stored = |p: &str| {
            canonical_folder_path(Path::new(p))
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        };
        let real = tmp.join("real").to_str().unwrap().to_string();
        assert_eq!(stored(&format!("{}/link/", tmp.display())), real);
        assert_eq!(
            stored(&format!("{}/link/./new//dir/", tmp.display())),
            format!("{real}/new/dir")
        );
    }

    #[test]
    fn a_path_that_is_not_utf8_is_unresolvable() {
        use std::os::unix::ffi::OsStrExt;
        let (_dir, tmp) = tmp();
        let name = std::ffi::OsStr::from_bytes(b"bad\xff");

        assert_eq!(
            canonical_folder_path(&tmp.join(name)),
            Err(FolderPathError::Unresolvable(
                "the path is not valid UTF-8".to_string()
            ))
        );
    }

    #[test]
    fn a_dangling_symlink_is_unresolvable() {
        let (_dir, tmp) = tmp();
        symlink(tmp.join("missing"), tmp.join("dangling")).unwrap();

        assert!(matches!(
            canonical_folder_path(&tmp.join("dangling/sub")),
            Err(FolderPathError::Unresolvable(_))
        ));
    }

    #[test]
    fn relative_and_parent_paths_are_refused() {
        let (_dir, tmp) = tmp();
        assert_eq!(
            canonical_folder_path(Path::new("music")),
            Err(FolderPathError::NotAbsolute)
        );
        assert_eq!(
            canonical_folder_path(&tmp.join("a/../b")),
            Err(FolderPathError::ParentComponent)
        );
    }

    #[test]
    fn equal_containing_and_inside_paths_of_another_user_are_refused() {
        let (_dir, tmp) = tmp();
        let folders = [pf("b", false, &tmp.join("music/bob"), false)];
        let roots = [tmp.clone()];
        let me = actor("a", Role::User);
        let check = |p: &str| check_folder_path(&tmp.join(p), &me, &folders, &roots);

        assert_eq!(
            check("music/bob"),
            Err(FolderRefusal::Overlaps(Overlap::Equal))
        );
        assert_eq!(
            check("music"),
            Err(FolderRefusal::Overlaps(Overlap::Contains))
        );
        assert_eq!(
            check("music/bob/x"),
            Err(FolderRefusal::Overlaps(Overlap::Inside))
        );
        assert_eq!(check("music/bobby"), Ok(()));
    }

    #[test]
    fn a_shared_folder_may_be_overlapped() {
        let (_dir, tmp) = tmp();
        let folders = [pf("b", false, &tmp.join("music/bob"), true)];
        let roots = [tmp.clone()];
        let me = actor("a", Role::User);

        for p in ["music/bob", "music", "music/bob/x"] {
            assert_eq!(
                check_folder_path(&tmp.join(p), &me, &folders, &roots),
                Ok(())
            );
        }
    }

    #[test]
    fn own_folders_may_overlap() {
        let (_dir, tmp) = tmp();
        let folders = [pf("a", false, &tmp.join("music"), false)];
        let roots = [tmp.clone()];

        assert_eq!(
            check_folder_path(
                &tmp.join("music/sub"),
                &actor("a", Role::User),
                &folders,
                &roots
            ),
            Ok(())
        );
    }

    #[test]
    fn non_admins_stay_under_approved_roots() {
        let (_dir, tmp) = tmp();
        let roots = [tmp.join("music")];
        let user = actor("a", Role::User);
        let admin = actor("z", Role::Admin);

        let outside = Err(FolderRefusal::OutsideApprovedRoots {
            roots: roots.to_vec(),
        });
        assert_eq!(
            check_folder_path(&tmp.join("elsewhere"), &user, &[], &roots),
            outside
        );
        assert_eq!(
            check_folder_path(&tmp.join("musical"), &user, &[], &roots),
            outside
        );
        assert_eq!(
            check_folder_path(&tmp.join("music/me"), &user, &[], &roots),
            Ok(())
        );
        assert_eq!(
            check_folder_path(&tmp.join("elsewhere"), &admin, &[], &roots),
            Ok(())
        );
    }

    #[test]
    fn a_root_itself_is_not_a_place_for_a_non_admin_folder() {
        let (_dir, tmp) = tmp();
        let roots = [tmp.join("users")];

        assert_eq!(
            check_folder_path(&tmp.join("users"), &actor("a", Role::User), &[], &roots),
            Err(FolderRefusal::OutsideApprovedRoots {
                roots: roots.to_vec()
            })
        );
        assert_eq!(
            check_folder_path(&tmp.join("users"), &actor("z", Role::Admin), &[], &roots),
            Ok(())
        );
    }

    #[test]
    fn the_roots_refusal_names_the_roots_and_sends_to_an_admin() {
        let some = FolderRefusal::OutsideApprovedRoots {
            roots: vec![PathBuf::from("/music"), PathBuf::from("/data/users")],
        };
        assert_eq!(
            some.to_string(),
            "This path is not inside a folder root an admin approved. Use a path inside /music, /data/users, or ask an admin to approve another root."
        );
        let none = FolderRefusal::OutsideApprovedRoots { roots: vec![] };
        assert_eq!(
            none.to_string(),
            "No folder roots are approved yet, so you cannot add a folder. Ask an admin to approve one."
        );
    }

    #[test]
    fn symlinked_root_still_approves_paths_below_it() {
        let (_dir, tmp) = tmp();
        std::fs::create_dir(tmp.join("real")).unwrap();
        symlink(tmp.join("real"), tmp.join("link")).unwrap();
        let roots = [canonical_folder_path(&tmp.join("link")).unwrap()];

        assert_eq!(
            check_folder_path(&tmp.join("real/me"), &actor("a", Role::User), &[], &roots),
            Ok(())
        );
    }

    #[test]
    fn a_symlink_into_another_users_folder_is_refused() {
        let (_dir, tmp) = tmp();
        std::fs::create_dir_all(tmp.join("music/bob")).unwrap();
        symlink(tmp.join("music/bob"), tmp.join("mine")).unwrap();
        let folders = [pf("b", false, &tmp.join("music/bob"), false)];
        let roots = [tmp.clone()];
        let candidate = canonical_folder_path(&tmp.join("mine")).unwrap();

        assert_eq!(
            check_folder_path(&candidate, &actor("a", Role::User), &folders, &roots),
            Err(FolderRefusal::Overlaps(Overlap::Equal))
        );
    }

    #[test]
    fn a_file_in_another_users_nested_folder_is_not_the_actors() {
        // Alice shared /music; Bob has /music/bob inside it.
        let (_dir, tmp) = tmp();
        let folders = [
            pf("alice", false, &tmp.join("music"), true),
            pf("bob", false, &tmp.join("music/bob"), false),
        ];
        let bobs_file = tmp.join("music/bob/B/y.flac");
        let alices_file = tmp.join("music/alice/A/x.flac");
        assert_eq!(file_owner(&bobs_file, &folders, "alice"), FileOwner::Other);
        assert_eq!(file_owner(&bobs_file, &folders, "bob"), FileOwner::Actor);
        assert_eq!(
            file_owner(&alices_file, &folders, "alice"),
            FileOwner::Actor
        );
        assert_eq!(file_owner(&alices_file, &folders, "bob"), FileOwner::Other);
        assert_eq!(
            file_owner(&tmp.join("elsewhere/z.flac"), &folders, "alice"),
            FileOwner::Other
        );
    }

    #[test]
    fn a_file_in_a_folder_two_users_share_is_neither_ones_alone() {
        let (_dir, tmp) = tmp();
        let folders = [
            pf("alice", false, &tmp.join("music"), true),
            pf("bob", false, &tmp.join("music"), false),
        ];
        let file = tmp.join("music/x.flac");
        assert_eq!(file_owner(&file, &folders, "alice"), FileOwner::Other);
        assert_eq!(file_owner(&file, &folders, "bob"), FileOwner::Other);
    }

    #[test]
    fn another_users_unresolvable_folder_protects_what_it_may_hold() {
        // Alice has /music; Bob's /music/bob cannot be resolved.
        let (_dir, tmp) = tmp();
        let bobs = tmp.join("music/bob");
        let folders = [
            pf("alice", false, &tmp.join("music"), false),
            unresolved("bob", bobs.to_str().unwrap()),
        ];
        assert_eq!(
            file_owner(&bobs.join("B/y.flac"), &folders, "alice"),
            FileOwner::Unresolved {
                folder: &bobs,
                error: "Too many levels of symbolic links",
            }
        );
        // Outside Bob's stored path, as written: still Alice's.
        assert_eq!(
            file_owner(&tmp.join("music/alice/A/x.flac"), &folders, "alice"),
            FileOwner::Actor
        );
        // Bob's unresolvable folder is no reason to keep a file from Bob;
        // this one sits in Alice's folder.
        assert_eq!(
            file_owner(&tmp.join("music/bob/B/y.flac"), &folders, "bob"),
            FileOwner::Other
        );
    }

    #[test]
    fn a_stored_path_that_cannot_be_compared_protects_every_file() {
        let (_dir, tmp) = tmp();
        let alices_file = tmp.join("music/alice/A/x.flac");
        for stored in [
            "music/bob".to_string(),
            format!("{}/music/alice/../bob", tmp.display()),
        ] {
            let folders = [
                pf("alice", false, &tmp.join("music"), false),
                unresolved("bob", &stored),
            ];
            assert!(
                matches!(
                    file_owner(&alices_file, &folders, "alice"),
                    FileOwner::Unresolved { .. }
                ),
                "{stored}"
            );
        }
    }

    #[test]
    fn an_unresolvable_folder_keeps_its_stored_path() {
        let (_dir, tmp) = tmp();
        symlink(tmp.join("loop"), tmp.join("loop")).unwrap();
        let stored = tmp.join("loop/bob");
        let row = OwnedFolder {
            folder: Folder {
                id: "f".to_string(),
                user_id: "bob".to_string(),
                name: "Bob".to_string(),
                path: stored.to_str().unwrap().to_string(),
                shared: false,
            },
            owner: "bob".to_string(),
            owner_role: Role::User,
        };
        let (folders, _) = policy_inputs(&[row], &[]);
        assert!(folders[0].path.is_err());
        assert_eq!(folders[0].stored_path, stored);
    }

    #[test]
    fn an_unresolvable_folder_blocks_the_check() {
        let (_dir, tmp) = tmp();
        let folders = [PolicyFolder {
            path: Err("permission denied".to_string()),
            ..pf("b", false, &tmp, false)
        }];
        let roots = [tmp.clone()];

        assert_eq!(
            check_folder_path(
                &tmp.join("anywhere"),
                &actor("a", Role::User),
                &folders,
                &roots
            ),
            Err(FolderRefusal::Unchecked)
        );
    }

    #[test]
    fn a_shared_unresolvable_folder_does_not_block_the_check() {
        let (_dir, tmp) = tmp();
        let folders = [PolicyFolder {
            path: Err("permission denied".to_string()),
            ..pf("b", false, &tmp, true)
        }];
        let roots = [tmp.clone()];

        assert_eq!(
            check_folder_path(
                &tmp.join("anywhere"),
                &actor("a", Role::User),
                &folders,
                &roots
            ),
            Ok(())
        );
    }

    #[test]
    fn a_folder_that_stays_in_place_passes_even_when_it_conflicts() {
        // Upgraded install: the admin's /music and Bob's /music/bob, no root.
        let (_dir, tmp) = tmp();
        let folders = [
            pf("z", true, &tmp.join("music"), false),
            pf("b", false, &tmp.join("music/bob"), false),
        ];
        let bob = actor("b", Role::User);
        let stored = format!("{}/music/bob/", tmp.display());

        assert_eq!(
            check_folder_update(&tmp.join("music/bob"), &stored, &bob, &folders, &[]),
            Ok(())
        );
        assert_eq!(
            check_folder_update(&tmp.join("music/bob2"), &stored, &bob, &folders, &[]),
            Err(FolderRefusal::OutsideApprovedRoots { roots: vec![] })
        );
        let admin_stored = tmp.join("music").to_str().unwrap().to_string();
        let admin = actor("z", Role::Admin);
        assert_eq!(
            check_folder_update(&tmp.join("music"), &admin_stored, &admin, &folders, &[]),
            Ok(())
        );
        assert_eq!(
            check_folder_update(&tmp, &admin_stored, &admin, &folders, &[]),
            Err(FolderRefusal::Overlaps(Overlap::Contains))
        );
    }

    #[test]
    fn conflicts_list_existing_overlaps_and_rootless_folders() {
        let (_dir, tmp) = tmp();
        let roots = [tmp.join("music")];
        let a = pf("a", true, &tmp.join("music"), false);
        let b = pf("b", false, &tmp.join("music/b"), false);

        assert_eq!(
            folder_conflicts(&[a.clone(), b.clone()], &roots),
            vec![
                FolderConflict {
                    folder: 0,
                    kind: ConflictKind::ContainsOther(1),
                },
                FolderConflict {
                    folder: 1,
                    kind: ConflictKind::InsideOther(0),
                },
            ]
        );

        let shared_a = PolicyFolder { shared: true, ..a };
        assert_eq!(folder_conflicts(&[shared_a, b], &roots), vec![]);

        let c = pf("c", false, &tmp.join("other"), false);
        let at_root = pf("d", false, &tmp.join("music"), false);
        assert_eq!(
            folder_conflicts(&[c, at_root], &roots),
            vec![
                FolderConflict {
                    folder: 0,
                    kind: ConflictKind::OutsideApprovedRoots,
                },
                FolderConflict {
                    folder: 1,
                    kind: ConflictKind::OutsideApprovedRoots,
                },
            ]
        );
    }

    #[test]
    fn conflicts_report_equal_and_unresolvable_folders() {
        let (_dir, tmp) = tmp();
        let roots = [tmp.clone()];
        let a = pf("a", false, &tmp.join("music"), false);
        let b = pf("b", false, &tmp.join("music"), false);
        let broken = PolicyFolder {
            path: Err("permission denied".to_string()),
            ..pf("x", false, &tmp, false)
        };

        assert_eq!(
            folder_conflicts(&[a, b, broken], &roots),
            vec![
                FolderConflict {
                    folder: 0,
                    kind: ConflictKind::SameAsOther(1),
                },
                FolderConflict {
                    folder: 1,
                    kind: ConflictKind::SameAsOther(0),
                },
                FolderConflict {
                    folder: 2,
                    kind: ConflictKind::Unresolvable {
                        error: "permission denied".into(),
                    },
                },
            ]
        );
    }

    #[test]
    fn policy_inputs_mark_admin_folders_and_drop_unresolvable_roots() {
        let (_dir, tmp) = tmp();
        std::fs::create_dir(tmp.join("music")).unwrap();
        symlink(tmp.join("music"), tmp.join("link")).unwrap();
        let folder = |id: &str, user: &str, role: Role, path: PathBuf| OwnedFolder {
            folder: Folder {
                id: id.into(),
                user_id: user.into(),
                name: id.into(),
                path: path.to_str().unwrap().into(),
                shared: false,
            },
            owner: user.into(),
            owner_role: role,
        };
        let root = |path: &str| FolderRoot {
            path: path.into(),
            created_at: String::new(),
        };

        let (folders, roots) = policy_inputs(
            &[
                folder("f1", "admin-id", Role::Admin, tmp.join("link")),
                folder("f2", "u1", Role::User, tmp.join("missing/x")),
            ],
            &[
                root(tmp.join("link").to_str().unwrap()),
                root("relative/root"),
            ],
        );

        assert!(folders[0].owner_is_admin);
        assert_eq!(folders[0].path, Ok(tmp.join("music")));
        assert!(!folders[1].owner_is_admin);
        assert_eq!(folders[1].path, Ok(tmp.join("missing/x")));
        assert_eq!(roots, vec![tmp.join("music")]);
    }

    #[tokio::test]
    async fn the_admin_view_names_every_folder_of_a_conflict_from_one_read() {
        use crate::db::test_db;
        let (_dir, tmp) = tmp();
        let db = test_db::migrated().await;
        let admin_id = crate::security::SEEDED_ADMIN_ID;
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ('b', 'bob', 'x')")
            .execute(&db)
            .await
            .unwrap();
        for (id, user, name, path) in [
            ("fa", admin_id, "Main", tmp.join("music")),
            ("fb", "b", "Mine", tmp.join("music/bob")),
        ] {
            sqlx::query("INSERT INTO folders (id, user_id, name, path) VALUES (?, ?, ?, ?)")
                .bind(id)
                .bind(user)
                .bind(name)
                .bind(path.to_str().unwrap())
                .execute(&db)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO folder_roots (path) VALUES (?)")
            .bind(tmp.to_str().unwrap())
            .execute(&db)
            .await
            .unwrap();

        let view = PolicyRows::load_in(&db).await.unwrap().admin_view();

        let label = |owner: &str, name: &str| FolderLabel {
            owner: owner.into(),
            name: name.into(),
        };
        assert_eq!(view.roots, vec![tmp.to_str().unwrap().to_string()]);
        assert_eq!(
            view.folders
                .iter()
                .map(|e| (e.owner.as_str(), e.folder.id.as_str()))
                .collect::<Vec<_>>(),
            vec![("admin", "fa"), ("bob", "fb")]
        );
        assert_eq!(
            view.conflicts,
            vec![
                FolderConflict {
                    folder: label("admin", "Main"),
                    kind: ConflictKind::ContainsOther(label("bob", "Mine")),
                },
                FolderConflict {
                    folder: label("bob", "Mine"),
                    kind: ConflictKind::InsideOther(label("admin", "Main")),
                },
            ]
        );
    }
}
