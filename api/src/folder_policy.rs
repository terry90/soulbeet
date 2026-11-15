//! Where a folder may live: canonical paths, approved roots for non-admins,
//! and no overlap with another user's folder unless that folder is shared.

use crate::models::folder::Folder;
#[cfg(feature = "server")]
use crate::models::folder_root::FolderRoot;
#[cfg(feature = "server")]
use crate::models::user::User;
use serde::{Deserialize, Serialize};

#[cfg(feature = "server")]
use crate::auth::UserCtx;
#[cfg(feature = "server")]
use std::collections::HashSet;
#[cfg(feature = "server")]
use std::fmt;
#[cfg(feature = "server")]
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ConflictKind {
    SameAsOther,
    ContainsOther,
    InsideOther,
    OutsideApprovedRoots,
    Unresolvable { error: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FolderConflict {
    pub folder_id: String,
    pub kind: ConflictKind,
    pub with_folder_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FolderAdminEntry {
    pub folder: Folder,
    pub owner: String,
    pub owner_is_admin: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FolderAdminView {
    pub roots: Vec<String>,
    pub folders: Vec<FolderAdminEntry>,
    pub conflicts: Vec<FolderConflict>,
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
    pub shared: bool,
}

#[cfg(feature = "server")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FolderRefusal {
    Path(FolderPathError),
    OutsideApprovedRoots,
    Overlaps(Overlap),
    Unchecked,
    Changed,
}

#[cfg(feature = "server")]
impl fmt::Display for FolderRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FolderRefusal::Path(e) => return e.fmt(f),
            FolderRefusal::OutsideApprovedRoots => {
                "This path is not under a folder root an admin approved (Settings > Library)"
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
                "An existing folder cannot be resolved, so this path cannot be checked. An admin can see it in Settings > Library."
            }
            FolderRefusal::Changed => "The folder path changed while it was being created",
        })
    }
}

#[cfg(feature = "server")]
fn under_a_root(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

/// Whether `actor` may keep a folder at the canonical path `candidate`.
#[cfg(feature = "server")]
pub fn check_folder_path(
    candidate: &Path,
    actor: &UserCtx,
    folders: &[PolicyFolder],
    roots: &[PathBuf],
) -> Result<(), FolderRefusal> {
    if !actor.role.is_admin() && !under_a_root(candidate, roots) {
        return Err(FolderRefusal::OutsideApprovedRoots);
    }
    for other in folders.iter().filter(|f| f.user_id != actor.user_id) {
        let path = other.path.as_ref().map_err(|_| FolderRefusal::Unchecked)?;
        if other.shared {
            continue;
        }
        if let Some(kind) = overlap(candidate, path) {
            return Err(FolderRefusal::Overlaps(kind));
        }
    }
    Ok(())
}

/// Every stored folder that breaks the rules today, for admins to sort out.
#[cfg(feature = "server")]
pub fn folder_conflicts(folders: &[PolicyFolder], roots: &[PathBuf]) -> Vec<FolderConflict> {
    let mut conflicts = Vec::new();
    for folder in folders {
        let path = match &folder.path {
            Ok(path) => path,
            Err(error) => {
                conflicts.push(FolderConflict {
                    folder_id: folder.id.clone(),
                    kind: ConflictKind::Unresolvable {
                        error: error.clone(),
                    },
                    with_folder_id: None,
                });
                continue;
            }
        };
        if !folder.owner_is_admin && !under_a_root(path, roots) {
            conflicts.push(FolderConflict {
                folder_id: folder.id.clone(),
                kind: ConflictKind::OutsideApprovedRoots,
                with_folder_id: None,
            });
        }
        if folder.shared {
            continue;
        }
        for other in folders {
            if other.user_id == folder.user_id || other.shared {
                continue;
            }
            let Ok(other_path) = &other.path else {
                continue;
            };
            let kind = match overlap(path, other_path) {
                Some(Overlap::Equal) => ConflictKind::SameAsOther,
                Some(Overlap::Contains) => ConflictKind::ContainsOther,
                Some(Overlap::Inside) => ConflictKind::InsideOther,
                None => continue,
            };
            conflicts.push(FolderConflict {
                folder_id: folder.id.clone(),
                kind,
                with_folder_id: Some(other.id.clone()),
            });
        }
    }
    conflicts
}

/// Canonicalizes every stored folder and root. A root that does not resolve
/// is left out, so it approves nothing.
#[cfg(feature = "server")]
pub(crate) fn policy_inputs(
    folders: Vec<Folder>,
    admin_ids: &HashSet<String>,
    roots: Vec<FolderRoot>,
) -> (Vec<PolicyFolder>, Vec<PathBuf>) {
    let folders = folders
        .into_iter()
        .map(|f| PolicyFolder {
            owner_is_admin: admin_ids.contains(&f.user_id),
            path: canonical_folder_path(Path::new(&f.path)).map_err(|e| e.to_string()),
            id: f.id,
            user_id: f.user_id,
            shared: f.shared,
        })
        .collect();
    let roots = roots
        .iter()
        .filter_map(|r| canonical_folder_path(Path::new(&r.path)).ok())
        .collect();
    (folders, roots)
}

#[cfg(feature = "server")]
pub async fn load_policy_inputs() -> Result<(Vec<PolicyFolder>, Vec<PathBuf>), String> {
    let folders = Folder::get_all().await?;
    let admin_ids = User::get_all()
        .await?
        .into_iter()
        .filter(|u| u.role.is_admin())
        .map(|u| u.id)
        .collect();
    let roots = FolderRoot::get_all().await?;
    Ok(policy_inputs(folders, &admin_ids, roots))
}

/// Startup re-validation: SQL cannot canonicalize, so existing rows are
/// checked here on every start.
#[cfg(feature = "server")]
pub async fn log_folder_conflicts() {
    match load_policy_inputs().await {
        Ok((folders, roots)) => {
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

    fn pf(id: &str, user: &str, admin: bool, path: &Path, shared: bool) -> PolicyFolder {
        PolicyFolder {
            id: id.to_string(),
            user_id: user.to_string(),
            owner_is_admin: admin,
            path: Ok(path.to_path_buf()),
            shared,
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
        let folders = [pf("bob", "b", false, &tmp.join("music/bob"), false)];
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
        let folders = [pf("bob", "b", false, &tmp.join("music/bob"), true)];
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
        let folders = [pf("mine", "a", false, &tmp.join("music"), false)];
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

        assert_eq!(
            check_folder_path(&tmp.join("elsewhere"), &user, &[], &roots),
            Err(FolderRefusal::OutsideApprovedRoots)
        );
        assert_eq!(
            check_folder_path(&tmp.join("musical"), &user, &[], &roots),
            Err(FolderRefusal::OutsideApprovedRoots)
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
        let folders = [pf("bob", "b", false, &tmp.join("music/bob"), false)];
        let roots = [tmp.clone()];
        let candidate = canonical_folder_path(&tmp.join("mine")).unwrap();

        assert_eq!(
            check_folder_path(&candidate, &actor("a", Role::User), &folders, &roots),
            Err(FolderRefusal::Overlaps(Overlap::Equal))
        );
    }

    #[test]
    fn an_unresolvable_folder_blocks_the_check() {
        let (_dir, tmp) = tmp();
        let folders = [PolicyFolder {
            path: Err("permission denied".to_string()),
            ..pf("bob", "b", false, &tmp, false)
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
    fn conflicts_list_existing_overlaps_and_rootless_folders() {
        let (_dir, tmp) = tmp();
        let roots = [tmp.join("music")];
        let a = pf("fa", "a", true, &tmp.join("music"), false);
        let b = pf("fb", "b", false, &tmp.join("music/b"), false);

        assert_eq!(
            folder_conflicts(&[a.clone(), b.clone()], &roots),
            vec![
                FolderConflict {
                    folder_id: "fa".into(),
                    kind: ConflictKind::ContainsOther,
                    with_folder_id: Some("fb".into()),
                },
                FolderConflict {
                    folder_id: "fb".into(),
                    kind: ConflictKind::InsideOther,
                    with_folder_id: Some("fa".into()),
                },
            ]
        );

        let shared_a = PolicyFolder { shared: true, ..a };
        assert_eq!(folder_conflicts(&[shared_a, b], &roots), vec![]);

        let c = pf("fc", "c", false, &tmp.join("other"), false);
        assert_eq!(
            folder_conflicts(&[c], &roots),
            vec![FolderConflict {
                folder_id: "fc".into(),
                kind: ConflictKind::OutsideApprovedRoots,
                with_folder_id: None,
            }]
        );
    }

    #[test]
    fn conflicts_report_equal_and_unresolvable_folders() {
        let (_dir, tmp) = tmp();
        let roots = [tmp.clone()];
        let a = pf("fa", "a", false, &tmp.join("music"), false);
        let b = pf("fb", "b", false, &tmp.join("music"), false);
        let broken = PolicyFolder {
            path: Err("permission denied".to_string()),
            ..pf("fx", "x", false, &tmp, false)
        };

        assert_eq!(
            folder_conflicts(&[a, b, broken], &roots),
            vec![
                FolderConflict {
                    folder_id: "fa".into(),
                    kind: ConflictKind::SameAsOther,
                    with_folder_id: Some("fb".into()),
                },
                FolderConflict {
                    folder_id: "fb".into(),
                    kind: ConflictKind::SameAsOther,
                    with_folder_id: Some("fa".into()),
                },
                FolderConflict {
                    folder_id: "fx".into(),
                    kind: ConflictKind::Unresolvable {
                        error: "permission denied".into(),
                    },
                    with_folder_id: None,
                },
            ]
        );
    }

    #[test]
    fn policy_inputs_mark_admin_folders_and_drop_unresolvable_roots() {
        let (_dir, tmp) = tmp();
        std::fs::create_dir(tmp.join("music")).unwrap();
        symlink(tmp.join("music"), tmp.join("link")).unwrap();
        let folder = |id: &str, user: &str, path: PathBuf| Folder {
            id: id.into(),
            user_id: user.into(),
            name: id.into(),
            path: path.to_str().unwrap().into(),
            shared: false,
        };
        let root = |path: &str| FolderRoot {
            path: path.into(),
            created_at: String::new(),
        };
        let admins = HashSet::from(["admin-id".to_string()]);

        let (folders, roots) = policy_inputs(
            vec![
                folder("f1", "admin-id", tmp.join("link")),
                folder("f2", "u1", tmp.join("missing/x")),
            ],
            &admins,
            vec![
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
}
