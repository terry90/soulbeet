//! Moving library tracks or whole albums between a user's folders without side effects.
//!
//! Preserves Navidrome song IDs, ratings, stars, play counts and static playlist memberships by:
//! 1. Never rewriting audio tags, file bytes, or filenames (Navidrome cross-library move matching
//!    relies on exact filename stem `utils.BaseName` and identical `size`/tags).
//! 2. Waiting for an idle Navidrome scanner, then moving atomically (rename on same FS; hidden
//!    non-audio `.tmp` copy + fsync + mtime preserve + rename + unlink across FS).
//! 3. Running the full multi-user delete guard whenever moving could hide a track from another
//!    listener (e.g. Shared -> Personal across libraries) or when `replace_existing` replaces a
//!    copy already in the target folder.
//! 4. Updating source (`forget`) and destination (`register_in_place`) beets DBs through
//!    `ImportQueue`.
//! 5. Triggering a full-library Navidrome scan, verifying `song_id` survived, learning the target
//!    library root, and restoring the caller's album rating/star on the original album if a
//!    partial-album cross-library move caused Navidrome's `ReassignAnnotation` to move it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use shared::failure::{cut_json, Failure, FailureCode, RetryAdvice, Service, Stage, TEXT_MAX_CHARS};
use shared::navidrome::DeletionReason;
use shared::recommendation::normalize_for_matching;
use soulbeet::navidrome::models::{NativeSong, SubsonicAlbum};
use soulbeet::navidrome::StarTarget;
use soulbeet::{ForgetOutcome, ImportedItem, RegisterOutcome};
use tracing::{info, warn};

use super::{
    atomic_move_file, find_sidecars, is_inside_discovery, transfer_sidecars, trigger_and_wait_scan,
    wait_for_idle_scan, ScanWaitOutcome, TrackSelection, ALBUM_MAX_TRACKS, IDLE_SCAN_WAIT_MAX,
    SCAN_WAIT_MAX, SONGS_PER_CALL,
};
use crate::auth::{UserCtx, UserId};
use crate::core::database_failure;
use crate::core::delete_guard::quota::deleting_app_in;
use crate::core::delete_guard::{
    forget_note, paths, rules, Checked, ForgetVia, Guard, Origin, Verdict,
};
use crate::core::folders::{self, folder_not_found};
use crate::core::index::{library_songs, NavidromeOf};
use crate::core::library::{
    album_not_found, known_roots_in, listing, place_in, FolderRef, Listing, SongFacts,
};
use crate::folder_policy::{canonical_folder_path, PolicyRows};
use crate::import_queue::ImportQueue;
use crate::models::discovery_playlist::DiscoveryTrackRow;
use crate::models::folder::Folder;
use crate::models::move_history::{MoveHistoryRow, MoveRecord};
use crate::models::user::User;

#[derive(Debug, Clone)]
pub struct MoveRequest {
    pub selection: TrackSelection,
    pub target_folder_id: String,
    pub dry_run: bool,
    pub replace_existing: bool,
    pub admin_access: bool,
    pub allow_delete: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct MoveFolderSummary {
    pub folder_id: String,
    pub name: String,
    pub path: String,
    pub shared: bool,
    pub in_navidrome: Listing,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_own: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AlbumSplitInfo {
    pub album_id: String,
    pub album_name: String,
    pub moving_tracks: usize,
    pub total_tracks: usize,
    pub album_rating: u8,
    pub album_starred: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplacedCopyInfo {
    pub song_id: String,
    pub title: String,
    pub format: Option<String>,
    pub bitrate_kbps: Option<u32>,
    pub size_bytes: Option<u64>,
    pub file: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MoveItemSuccess {
    pub song_id: String,
    pub new_song_id: Option<String>,
    pub song_id_preserved: Option<bool>,
    /// `"would_move"` (dry_run), `"moved"`, `"moved_pending_scan"`, or `"moved_id_changed"`.
    pub status: String,
    pub song: SongFacts,
    pub source_folder: FolderRef,
    pub source_file: String,
    pub target_folder: FolderRef,
    pub target_file: String,
    pub crosses_navidrome_library: bool,
    pub album_split: Option<AlbumSplitInfo>,
    pub album_annotation_restored: bool,
    pub replaced: Vec<ReplacedCopyInfo>,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub struct MoveEntry {
    pub song_id: String,
    pub outcome: Result<MoveItemSuccess, MoveItemError>,
}

#[derive(Debug)]
pub struct MoveItemError {
    pub failure: Failure,
    pub song: Option<SongFacts>,
    pub existing_in_target: Vec<SongFacts>,
    pub discovery_track_id: Option<String>,
}

#[derive(Debug)]
pub(crate) struct MoveReport {
    pub dry_run: bool,
    pub target_folder: MoveFolderSummary,
    pub album_id: Option<String>,
    pub scan_status: Option<String>,
    pub entries: Vec<MoveEntry>,
}

struct PlannedMove {
    song: NativeSong,
    reader_user: UserId,
    source_folder_ref: FolderRef,
    source_folder_root: PathBuf,
    source_file: PathBuf,
    target_file: PathBuf,
    crosses_navidrome_library: bool,
    album_split: Option<AlbumSplitInfo>,
    album_annotation: Option<(String, u8, bool)>,
    replace_targets: Vec<Checked>,
    warnings: Vec<String>,
}

/// Checks and (when `!req.dry_run`) executes moving tracks or an album to `req.target_folder_id`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn move_tracks(
    guard: &Guard<'_>,
    navidrome: &dyn NavidromeOf,
    queue: &ImportQueue,
    ctx: &UserCtx,
    origin: &Origin,
    req: MoveRequest,
) -> Result<MoveReport, Failure> {
    let user = ctx.id();
    let target_folder = Folder::get_by_id_in(guard.db, &req.target_folder_id)
        .await
        .map_err(|e| database_failure(Stage::Move, e))?
        .ok_or_else(folder_not_found)?;
    if target_folder.user_id != ctx.user_id && !req.admin_access {
        return Err(if ctx.role.is_admin() {
            folders::admin_access_required(ctx, req.allow_delete, Stage::Move, true)
        } else {
            folder_not_found()
        });
    }
    let target_root = canonical_folder_path(Path::new(&target_folder.path)).map_err(|e| {
        Failure::new(
            FailureCode::InvalidInput,
            Stage::Move,
            RetryAdvice::NotRetryable,
            format!("Target folder path cannot be resolved: {e}"),
        )
    })?;
    let rows = PolicyRows::load_in(guard.db)
        .await
        .map_err(|e| database_failure(Stage::Move, e))?;
    let (policy_folders, _) = rows.inputs();
    if is_inside_discovery(&target_root, &policy_folders) {
        return Err(Failure::new(
            FailureCode::InvalidInput,
            Stage::Move,
            RetryAdvice::NotRetryable,
            "Cannot move library tracks into a Discovery subfolder",
        ));
    }

    let known_roots = known_roots_in(guard.db, guard.music_path).await?;
    let library_roots: Vec<PathBuf> = known_roots.iter().map(|(_, r)| r.clone()).collect();
    let target_place = place_in(&target_root, &known_roots);
    let target_listing = listing(&target_root, &library_roots);
    let target_owner_name = rows
        .folders
        .iter()
        .find(|r| r.folder.id == target_folder.id)
        .map(|r| cut_json(r.owner.clone(), TEXT_MAX_CHARS));
    let target_is_own = target_folder.user_id == ctx.user_id;
    let target_summary = MoveFolderSummary {
        folder_id: target_folder.id.clone(),
        name: cut_json(target_folder.name.clone(), TEXT_MAX_CHARS),
        path: target_root.to_string_lossy().into_owned(),
        shared: target_folder.shared,
        in_navidrome: target_listing,
        owner: req.admin_access.then_some(target_owner_name.clone()).flatten(),
        is_own: req.admin_access.then_some(target_is_own),
    };
    let target_ref = FolderRef {
        folder_id: target_folder.id.clone(),
        name: target_summary.name.clone(),
        shared: target_folder.shared,
        owner: target_summary.owner.clone(),
        is_own: target_summary.is_own,
    };

    let linked_users = User::linked_to_navidrome_in(guard.db)
        .await
        .map_err(|e| database_failure(Stage::Move, e))?;
    let other_users: Vec<UserId> = linked_users
        .into_iter()
        .filter(|u| u.id != ctx.user_id)
        .map(|u| UserId::new(&u.id))
        .collect();

    // Resolve selection into song_ids.
    let (song_ids, album_id) = match &req.selection {
        TrackSelection::Songs(ids) => {
            if ids.is_empty() || ids.len() > SONGS_PER_CALL {
                return Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Input,
                    RetryAdvice::NotRetryable,
                    format!("Provide between 1 and {SONGS_PER_CALL} distinct song ids"),
                ));
            }
            (ids.clone(), None)
        }
        TrackSelection::Album(aid) => {
            let mut album = guard
                .listeners
                .album(&user, aid)
                .await
                .map_err(|e| e.failure(Stage::Move, e.to_string()))?;
            if album.is_none() && req.admin_access {
                for other in &other_users {
                    if let Ok(Some(found)) = guard.listeners.album(other, aid).await {
                        album = Some(found);
                        break;
                    }
                }
            }
            let album = album.ok_or_else(|| album_not_found(aid))?;
            if album.song.is_empty() {
                return Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Move,
                    RetryAdvice::NotRetryable,
                    "This album has no songs to move",
                ));
            }
            if album.song.len() > ALBUM_MAX_TRACKS {
                return Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Input,
                    RetryAdvice::NotRetryable,
                    format!(
                        "This album has {} tracks, over the {ALBUM_MAX_TRACKS}-track limit for one move; move it in batches with song_ids",
                        album.song.len()
                    ),
                ));
            }
            let ids: Vec<String> = album.song.into_iter().map(|s| s.id).collect();
            (ids, Some(aid.clone()))
        }
    };

    // Confirm grant permission on DB before executing real moves.
    let app_host = if !req.dry_run {
        match origin {
            Origin::Mcp { grant_id } => {
                let mut conn = guard
                    .db
                    .acquire()
                    .await
                    .map_err(|e| database_failure(Stage::Move, e))?;
                let host = deleting_app_in(&mut conn, &user, grant_id)
                    .await
                    .map_err(|f| Failure {
                        stage: Stage::Move,
                        ..f
                    })?;
                Some(host)
            }
            Origin::Ui | Origin::Auto => None,
        }
    } else {
        None
    };

    let pending_discovery = DiscoveryTrackRow::pending_for_user_in(guard.db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Move, e))?;
    let mut owner_pending_cache: HashMap<String, Vec<shared::navidrome::DiscoveryTrack>> =
        HashMap::new();

    // Pre-fetch all native songs so we know how many tracks per album are in this batch.
    // When admin_access is enabled and the caller's Navidrome account cannot see a song
    // in another user's private library, fall back to reading it via other linked accounts.
    type FetchedSong = (String, Result<(NativeSong, UserId), Failure>);
    let mut fetched: Vec<FetchedSong> = Vec::with_capacity(song_ids.len());
    let mut batch_count_by_album: HashMap<String, usize> = HashMap::new();
    for song_id in &song_ids {
        match guard.read_song(&user, song_id).await {
            Ok(Some(song)) => {
                if !song.album_id.is_empty() {
                    *batch_count_by_album.entry(song.album_id.clone()).or_default() += 1;
                }
                fetched.push((song_id.clone(), Ok((song, user.clone()))));
            }
            Ok(None) if req.admin_access => {
                let mut found_other = None;
                for other in &other_users {
                    if let Ok(Some(song)) = guard.read_song(other, song_id).await {
                        found_other = Some((song, other.clone()));
                        break;
                    }
                }
                match found_other {
                    Some((song, reader)) => {
                        if !song.album_id.is_empty() {
                            *batch_count_by_album.entry(song.album_id.clone()).or_default() += 1;
                        }
                        fetched.push((song_id.clone(), Ok((song, reader))));
                    }
                    None => {
                        fetched.push((
                            song_id.clone(),
                            Err(Failure::new(
                                FailureCode::UnknownHandle,
                                Stage::Move,
                                RetryAdvice::NotRetryable,
                                "Navidrome has no song with this id that any linked account can see",
                            )),
                        ));
                    }
                }
            }
            Ok(None) => {
                fetched.push((
                    song_id.clone(),
                    Err(Failure::new(
                        FailureCode::UnknownHandle,
                        Stage::Move,
                        RetryAdvice::NotRetryable,
                        "Navidrome has no song with this id that this account can see",
                    )),
                ));
            }
            Err(f) => fetched.push((song_id.clone(), Err(Failure { stage: Stage::Move, ..f }))),
        }
    }

    let mut album_cache: HashMap<String, Option<SubsonicAlbum>> = HashMap::new();
    let mut planned: Vec<(usize, PlannedMove)> = Vec::new();
    let mut entries: Vec<Option<MoveEntry>> = (0..fetched.len()).map(|_| None).collect();
    let mut planned_destinations: HashSet<PathBuf> = HashSet::new();

    for (idx, (song_id, song_res)) in fetched.into_iter().enumerate() {
        let (song, reader_user) = match song_res {
            Ok(pair) => pair,
            Err(f) => {
                entries[idx] = Some(MoveEntry {
                    song_id,
                    outcome: Err(MoveItemError {
                        failure: f,
                        song: None,
                        existing_in_target: vec![],
                        discovery_track_id: None,
                    }),
                });
                continue;
            }
        };

        // Pending discovery track? Direct caller to promote_discovery_track(target_folder_id).
        if let Some(track) = pending_discovery
            .iter()
            .find(|t| t.song_id.as_deref() == Some(song.id.as_str()))
        {
            entries[idx] = Some(MoveEntry {
                song_id,
                outcome: Err(MoveItemError {
                    failure: Failure::new(
                        FailureCode::InvalidInput,
                        Stage::Move,
                        RetryAdvice::NotRetryable,
                        "This is a pending discovery track; use promote_discovery_track with target_folder_id instead",
                    ),
                    song: Some(SongFacts::from(&song)),
                    existing_in_target: vec![],
                    discovery_track_id: Some(track.id.clone()),
                }),
            });
            continue;
        }

        let (source_file, _file_id) =
            match paths::local_file_learning(guard.db, &song, guard.music_path, &reader_user).await
            {
                Ok(found) => found,
                Err(f) => {
                    entries[idx] = Some(MoveEntry {
                        song_id,
                        outcome: Err(MoveItemError {
                            failure: Failure { stage: Stage::Move, ..f },
                            song: Some(SongFacts::from(&song)),
                            existing_in_target: vec![],
                            discovery_track_id: None,
                        }),
                    });
                    continue;
                }
            };

        if let Some(track) = pending_discovery.iter().find(|t| {
            std::fs::canonicalize(&t.path).is_ok_and(|staged| staged == source_file)
        }) {
            entries[idx] = Some(MoveEntry {
                song_id,
                outcome: Err(MoveItemError {
                    failure: Failure::new(
                        FailureCode::InvalidInput,
                        Stage::Move,
                        RetryAdvice::NotRetryable,
                        "This is a pending discovery track; use promote_discovery_track with target_folder_id instead",
                    ),
                    song: Some(SongFacts::from(&song)),
                    existing_in_target: vec![],
                    discovery_track_id: Some(track.id.clone()),
                }),
            });
            continue;
        }

        let (source_root, source_folder_row, source_owner_name) = if req.admin_access {
            let (pf, root) = match rules::deepest_registered_folder(&source_file, &policy_folders, &user) {
                Ok(found) => found,
                Err(f) => {
                    entries[idx] = Some(MoveEntry {
                        song_id,
                        outcome: Err(MoveItemError {
                            failure: Failure { stage: Stage::Move, ..f },
                            song: Some(SongFacts::from(&song)),
                            existing_in_target: vec![],
                            discovery_track_id: None,
                        }),
                    });
                    continue;
                }
            };
            let Some(owned) = rows.folders.iter().find(|r| r.folder.id == pf.id) else {
                entries[idx] = Some(MoveEntry {
                    song_id,
                    outcome: Err(MoveItemError {
                        failure: Failure::new(
                            FailureCode::DeleteOutsideLibrary,
                            Stage::Move,
                            RetryAdvice::NotRetryable,
                            "The file is not inside any registered library folder",
                        ),
                        song: Some(SongFacts::from(&song)),
                        existing_in_target: vec![],
                        discovery_track_id: None,
                    }),
                });
                continue;
            };
            (
                root.to_path_buf(),
                &owned.folder,
                Some(cut_json(owned.owner.clone(), TEXT_MAX_CHARS)),
            )
        } else {
            let source_root = match rules::owning_folders(&source_file, &policy_folders, &user) {
                Ok(root) => root.to_path_buf(),
                Err(f) => {
                    let failure = if ctx.role.is_admin()
                        && rules::deepest_registered_folder(&source_file, &policy_folders, &user)
                            .is_ok()
                    {
                        folders::admin_access_required(ctx, req.allow_delete, Stage::Move, true)
                    } else {
                        Failure { stage: Stage::Move, ..f }
                    };
                    entries[idx] = Some(MoveEntry {
                        song_id,
                        outcome: Err(MoveItemError {
                            failure,
                            song: Some(SongFacts::from(&song)),
                            existing_in_target: vec![],
                            discovery_track_id: None,
                        }),
                    });
                    continue;
                }
            };
            let Some(folder_row) = find_user_folder(&rows, &ctx.user_id, &source_root) else {
                entries[idx] = Some(MoveEntry {
                    song_id,
                    outcome: Err(MoveItemError {
                        failure: Failure::new(
                            FailureCode::DeleteOutsideLibrary,
                            Stage::Move,
                            RetryAdvice::NotRetryable,
                            "The file is not inside one of your library folders",
                        ),
                        song: Some(SongFacts::from(&song)),
                        existing_in_target: vec![],
                        discovery_track_id: None,
                    }),
                });
                continue;
            };
            (source_root, folder_row, None)
        };

        // Also check pending_discovery for the source folder's owner when moving across users.
        if source_folder_row.user_id != ctx.user_id {
            if !owner_pending_cache.contains_key(&source_folder_row.user_id) {
                let owner_pending =
                    DiscoveryTrackRow::pending_for_user_in(guard.db, &source_folder_row.user_id)
                        .await
                        .map_err(|e| database_failure(Stage::Move, e))?;
                owner_pending_cache.insert(source_folder_row.user_id.clone(), owner_pending);
            }
            if let Some(track) = owner_pending_cache
                .get(&source_folder_row.user_id)
                .and_then(|list| {
                    list.iter().find(|t| {
                        t.song_id.as_deref() == Some(song.id.as_str())
                            || std::fs::canonicalize(&t.path)
                                .is_ok_and(|staged| staged == source_file)
                    })
                })
            {
                entries[idx] = Some(MoveEntry {
                    song_id,
                    outcome: Err(MoveItemError {
                        failure: Failure::new(
                            FailureCode::InvalidInput,
                            Stage::Move,
                            RetryAdvice::NotRetryable,
                            "This is a pending discovery track in the source folder owner's staging area; promote or remove it first",
                        ),
                        song: Some(SongFacts::from(&song)),
                        existing_in_target: vec![],
                        discovery_track_id: Some(track.id.clone()),
                    }),
                });
                continue;
            }
        }

        if let Err(f) = rules::audio_file(&source_file) {
            entries[idx] = Some(MoveEntry {
                song_id,
                outcome: Err(MoveItemError {
                    failure: Failure { stage: Stage::Move, ..f },
                    song: Some(SongFacts::from(&song)),
                    existing_in_target: vec![],
                    discovery_track_id: None,
                }),
            });
            continue;
        }

        let source_is_own = source_folder_row.user_id == ctx.user_id;
        let source_folder_ref = FolderRef {
            folder_id: source_folder_row.id.clone(),
            name: cut_json(source_folder_row.name.clone(), TEXT_MAX_CHARS),
            shared: source_folder_row.shared,
            owner: req.admin_access.then_some(source_owner_name).flatten(),
            is_own: req.admin_access.then_some(source_is_own),
        };

        if source_root == target_root {
            let mut sf = SongFacts::from(&song);
            sf.folder = Some(source_folder_ref);
            entries[idx] = Some(MoveEntry {
                song_id,
                outcome: Err(MoveItemError {
                    failure: Failure::new(
                        FailureCode::MoveSameFolder,
                        Stage::Move,
                        RetryAdvice::NotRetryable,
                        "This track is already in the target folder",
                    ),
                    song: Some(sf),
                    existing_in_target: vec![],
                    discovery_track_id: None,
                }),
            });
            continue;
        }

        // Check in-use jobs on both source holding folders and target folder.
        let mut watch_folders = rules::holding_folders(&source_file, &policy_folders);
        if !watch_folders.contains(&target_folder.id.as_str()) {
            watch_folders.push(target_folder.id.as_str());
        }
        if let Err(f) = rules::not_in_use_in(guard.db, &watch_folders, &song, &[]).await {
            let mut sf = SongFacts::from(&song);
            sf.folder = Some(source_folder_ref);
            entries[idx] = Some(MoveEntry {
                song_id,
                outcome: Err(MoveItemError {
                    failure: Failure { stage: Stage::Move, ..f },
                    song: Some(sf),
                    existing_in_target: vec![],
                    discovery_track_id: None,
                }),
            });
            continue;
        }

        let Ok(rel_path) = source_file.strip_prefix(&source_root) else {
            entries[idx] = Some(MoveEntry {
                song_id,
                outcome: Err(MoveItemError {
                    failure: Failure::new(
                        FailureCode::DeletePathUnresolved,
                        Stage::Move,
                        RetryAdvice::NotRetryable,
                        "Could not compute the track's relative path inside its source folder",
                    ),
                    song: Some(SongFacts::from(&song)),
                    existing_in_target: vec![],
                    discovery_track_id: None,
                }),
            });
            continue;
        };
        let target_file = target_root.join(rel_path);

        if !planned_destinations.insert(target_file.clone()) {
            let mut sf = SongFacts::from(&song);
            sf.folder = Some(source_folder_ref);
            entries[idx] = Some(MoveEntry {
                song_id,
                outcome: Err(MoveItemError {
                    failure: Failure::new(
                        FailureCode::MoveTargetExists,
                        Stage::Move,
                        RetryAdvice::NotRetryable,
                        "Another track in this move already maps to the same destination path",
                    ),
                    song: Some(sf),
                    existing_in_target: vec![],
                    discovery_track_id: None,
                }),
            });
            continue;
        }

        // Check destination file collision and logical duplicates already in target_root.
        let mut existing_in_target = find_target_duplicates(
            guard,
            &user,
            &song,
            &target_root,
            &target_file,
        )
        .await?;
        if req.admin_access && target_folder.user_id != ctx.user_id {
            let target_owner = UserId::new(&target_folder.user_id);
            if let Ok(extra) =
                find_target_duplicates(guard, &target_owner, &song, &target_root, &target_file)
                    .await
            {
                for dup in extra {
                    if !existing_in_target.iter().any(|e| e.id == dup.id) {
                        existing_in_target.push(dup);
                    }
                }
            }
        }
        let path_exists = tokio::fs::try_exists(&target_file)
            .await
            .map_err(|e| {
                Failure::new(
                    FailureCode::MoveFailed,
                    Stage::Move,
                    RetryAdvice::SameSourceLater,
                    "Could not check whether the destination file already exists",
                )
                .with_cause(Service::Filesystem, None, e.to_string())
            })?;

        let mut replace_targets: Vec<Checked> = Vec::new();
        if path_exists || !existing_in_target.is_empty() {
            let mut sf = SongFacts::from(&song);
            sf.folder = Some(source_folder_ref.clone());
            if !req.replace_existing {
                let (code, msg) = if !existing_in_target.is_empty() {
                    (
                        FailureCode::MoveDuplicateInTarget,
                        "The target folder already holds a copy of this track; compare the copies or pass replace_existing: true to replace it",
                    )
                } else {
                    (
                        FailureCode::MoveTargetExists,
                        "A file already exists at the destination path in the target folder",
                    )
                };
                entries[idx] = Some(MoveEntry {
                    song_id,
                    outcome: Err(MoveItemError {
                        failure: Failure::new(code, Stage::Move, RetryAdvice::NotRetryable, msg),
                        song: Some(sf),
                        existing_in_target: existing_in_target
                            .iter()
                            .map(|s| {
                                let mut f = SongFacts::from(s);
                                f.folder = Some(target_ref.clone());
                                f
                            })
                            .collect(),
                        discovery_track_id: None,
                    }),
                });
                continue;
            }
            // replace_existing == true: every copy to replace MUST pass the full delete_guard!
            if existing_in_target.is_empty() && path_exists {
                entries[idx] = Some(MoveEntry {
                    song_id,
                    outcome: Err(MoveItemError {
                        failure: Failure::new(
                            FailureCode::MoveTargetExists,
                            Stage::Move,
                            RetryAdvice::NotRetryable,
                            "A file already exists at the destination path and is not a Navidrome track Soulbeet can verify for replacement",
                        ),
                        song: Some(sf),
                        existing_in_target: vec![],
                        discovery_track_id: None,
                    }),
                });
                continue;
            }
            let mut guard_err = None;
            for dup in &existing_in_target {
                match guard.check_record(&user, dup.clone()).await {
                    Ok(Verdict::Library(checked)) => replace_targets.push(checked),
                    Ok(Verdict::Discovery { .. }) => {
                        guard_err = Some(Failure::new(
                            FailureCode::MoveDuplicateInTarget,
                            Stage::Move,
                            RetryAdvice::NotRetryable,
                            "The existing copy in the target folder is a pending discovery track; remove or promote it first",
                        ));
                        break;
                    }
                    Err(f) => {
                        guard_err = Some(Failure { stage: Stage::Move, ..f });
                        break;
                    }
                }
            }
            if let Some(f) = guard_err {
                entries[idx] = Some(MoveEntry {
                    song_id,
                    outcome: Err(MoveItemError {
                        failure: f,
                        song: Some(sf),
                        existing_in_target: existing_in_target
                            .iter()
                            .map(|s| {
                                let mut f = SongFacts::from(s);
                                f.folder = Some(target_ref.clone());
                                f
                            })
                            .collect(),
                        discovery_track_id: None,
                    }),
                });
                continue;
            }
        }

        // Cross-library check & multi-user visibility-shrinking veto!
        let same_navidrome_library = target_place
            .as_ref()
            .is_some_and(|p| p.library_id == song.library_id);
        let crosses_navidrome_library = !same_navidrome_library;

        if crosses_navidrome_library {
            let other_folder_overlaps_source = policy_folders.iter().any(|f| {
                f.user_id != ctx.user_id
                    && f.path
                        .as_deref()
                        .is_ok_and(|p| p == source_root || source_file.starts_with(p))
            });
            let mut another_user_sees_song = false;
            if !source_folder_row.shared && !other_folder_overlaps_source {
                for other in &other_users {
                    if guard
                        .listeners
                        .song(other, &song.id)
                        .await
                        .ok()
                        .flatten()
                        .is_some()
                    {
                        another_user_sees_song = true;
                        break;
                    }
                }
            }
            // If moving out of a shared/multi-user visible source across libraries (or into an
            // unknown library), another listener could lose access -> run full multi-user veto!
            let may_hide_from_others = source_folder_row.shared
                || other_folder_overlaps_source
                || another_user_sees_song;
            if may_hide_from_others && !target_folder.shared {
                if let Err(f) = check_multi_user_veto(
                    guard,
                    &user,
                    &ctx.username,
                    &reader_user,
                    &song,
                    &source_file,
                )
                .await
                {
                    let mut sf = SongFacts::from(&song);
                    sf.folder = Some(source_folder_ref);
                    entries[idx] = Some(MoveEntry {
                        song_id,
                        outcome: Err(MoveItemError {
                            failure: Failure { stage: Stage::Move, ..f },
                            song: Some(sf),
                            existing_in_target: vec![],
                            discovery_track_id: None,
                        }),
                    });
                    continue;
                }
            }
        }

        let mut warnings = Vec::new();
        if target_listing == Listing::Unknown {
            warnings.push(
                "Target folder is not inside a Navidrome library Soulbeet has located yet; after scanning, Soulbeet will learn its root if Navidrome indexes it".into(),
            );
        }

        // Partial-album split warning & annotation backup.
        let mut album_split = None;
        let mut album_annotation = None;
        if crosses_navidrome_library && !song.album_id.is_empty() {
            if !album_cache.contains_key(&song.album_id) {
                let fetched_album = guard
                    .listeners
                    .album(&reader_user, &song.album_id)
                    .await
                    .ok()
                    .flatten();
                album_cache.insert(song.album_id.clone(), fetched_album);
            }
            if let Some(Some(album)) = album_cache.get(&song.album_id) {
                let moving_tracks = batch_count_by_album
                    .get(&song.album_id)
                    .copied()
                    .unwrap_or(1);
                let total_tracks = album.song.len();
                let rating = album.user_rating.unwrap_or(0);
                let starred = album.starred.is_some();
                if total_tracks > moving_tracks {
                    let album_name = cut_json(album.display_name().to_string(), TEXT_MAX_CHARS);
                    if rating > 0 || starred {
                        album_annotation = Some((album.id.clone(), rating, starred));
                    }
                    warnings.push(format!(
                        "Moving {moving_tracks} of {total_tracks} tracks from album '{album_name}' across Navidrome libraries will split the album across two libraries in Navidrome; pass album_id to move the whole album instead"
                    ));
                    album_split = Some(AlbumSplitInfo {
                        album_id: album.id.clone(),
                        album_name,
                        moving_tracks,
                        total_tracks,
                        album_rating: rating,
                        album_starred: starred,
                    });
                }
            }
        }

        planned.push((
            idx,
            PlannedMove {
                song,
                reader_user,
                source_folder_ref,
                source_folder_root: source_root,
                source_file,
                target_file,
                crosses_navidrome_library,
                album_split,
                album_annotation,
                replace_targets,
                warnings,
            },
        ));
    }

    // Dry run: return planned results immediately without touching disk, beets, or Navidrome.
    if req.dry_run {
        for (idx, p) in planned {
            let mut sf = SongFacts::from(&p.song);
            sf.folder = Some(p.source_folder_ref.clone());
            let replaced: Vec<ReplacedCopyInfo> = p
                .replace_targets
                .iter()
                .map(|c| ReplacedCopyInfo {
                    song_id: c.song.id.clone(),
                    title: cut_json(c.song.title.clone(), TEXT_MAX_CHARS),
                    format: (!c.song.suffix.is_empty()).then(|| c.song.suffix.clone()),
                    bitrate_kbps: (c.song.bit_rate > 0).then_some(c.song.bit_rate),
                    size_bytes: (c.song.size > 0).then_some(c.song.size),
                    file: c.path.to_string_lossy().into_owned(),
                })
                .collect();
            entries[idx] = Some(MoveEntry {
                song_id: p.song.id.clone(),
                outcome: Ok(MoveItemSuccess {
                    song_id: p.song.id,
                    new_song_id: None,
                    song_id_preserved: None,
                    status: "would_move".into(),
                    song: sf,
                    source_folder: p.source_folder_ref,
                    source_file: p.source_file.to_string_lossy().into_owned(),
                    target_folder: target_ref.clone(),
                    target_file: p.target_file.to_string_lossy().into_owned(),
                    crosses_navidrome_library: p.crosses_navidrome_library,
                    album_split: p.album_split,
                    album_annotation_restored: false,
                    replaced,
                    warnings: p.warnings,
                }),
            });
        }
        return Ok(MoveReport {
            dry_run: true,
            target_folder: target_summary,
            album_id,
            scan_status: None,
            entries: entries.into_iter().flatten().collect(),
        });
    }

    if planned.is_empty() {
        return Ok(MoveReport {
            dry_run: false,
            target_folder: target_summary,
            album_id,
            scan_status: None,
            entries: entries.into_iter().flatten().collect(),
        });
    }

    // Wait for any active Navidrome scan to finish before moving files.
    let own_navi = navidrome
        .navidrome_of(&user)
        .await
        .map_err(|e| e.failure(Stage::Move, e.to_string()))?;
    let _ = wait_for_idle_scan(&own_navi, IDLE_SCAN_WAIT_MAX).await;

    struct MovedOnDisk {
        idx: usize,
        plan: PlannedMove,
        replaced: Vec<ReplacedCopyInfo>,
        warnings: Vec<String>,
    }

    let mut moved: Vec<MovedOnDisk> = Vec::with_capacity(planned.len());
    for (idx, mut plan) in planned {
        let mut warnings = std::mem::take(&mut plan.warnings);
        let mut replaced = Vec::new();

        // Replace existing target duplicates through the full delete_guard first.
        let mut replace_failed = None;
        for checked in std::mem::take(&mut plan.replace_targets) {
            let info = ReplacedCopyInfo {
                song_id: checked.song.id.clone(),
                title: cut_json(checked.song.title.clone(), TEXT_MAX_CHARS),
                format: (!checked.song.suffix.is_empty()).then(|| checked.song.suffix.clone()),
                bitrate_kbps: (checked.song.bit_rate > 0).then_some(checked.song.bit_rate),
                size_bytes: (checked.song.size > 0).then_some(checked.song.size),
                file: checked.path.to_string_lossy().into_owned(),
            };
            match guard
                .carry_out(
                    &user,
                    checked,
                    DeletionReason::Replaced,
                    origin,
                    ForgetVia::Queue(queue.clone()),
                )
                .await
            {
                Ok(w) => {
                    warnings.extend(w);
                    replaced.push(info);
                }
                Err(not_removed) => {
                    replace_failed = Some(Failure {
                        stage: Stage::Move,
                        ..not_removed.into_failure()
                    });
                    break;
                }
            }
        }
        if let Some(f) = replace_failed {
            let mut sf = SongFacts::from(&plan.song);
            sf.folder = Some(plan.source_folder_ref);
            entries[idx] = Some(MoveEntry {
                song_id: plan.song.id,
                outcome: Err(MoveItemError {
                    failure: f,
                    song: Some(sf),
                    existing_in_target: vec![],
                    discovery_track_id: None,
                }),
            });
            continue;
        }

        // Inspect sidecars BEFORE moving the audio file.
        let sidecars = find_sidecars(&plan.source_file);
        let src_for_move = plan.source_file.clone();
        let dst_for_move = plan.target_file.clone();
        let move_res =
            tokio::task::spawn_blocking(move || atomic_move_file(&src_for_move, &dst_for_move))
                .await;
        match move_res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let code = if e.kind() == std::io::ErrorKind::AlreadyExists {
                    FailureCode::MoveTargetExists
                } else {
                    FailureCode::MoveFailed
                };
                let mut sf = SongFacts::from(&plan.song);
                sf.folder = Some(plan.source_folder_ref);
                entries[idx] = Some(MoveEntry {
                    song_id: plan.song.id,
                    outcome: Err(MoveItemError {
                        failure: Failure::new(
                            code,
                            Stage::Move,
                            RetryAdvice::SameSourceLater,
                            format!("Could not move file: {e}"),
                        )
                        .with_cause(Service::Filesystem, None, e.to_string()),
                        song: Some(sf),
                        existing_in_target: vec![],
                        discovery_track_id: None,
                    }),
                });
                continue;
            }
            Err(e) => {
                let mut sf = SongFacts::from(&plan.song);
                sf.folder = Some(plan.source_folder_ref);
                entries[idx] = Some(MoveEntry {
                    song_id: plan.song.id,
                    outcome: Err(MoveItemError {
                        failure: Failure::new(
                            FailureCode::MoveFailed,
                            Stage::Move,
                            RetryAdvice::SameSourceLater,
                            format!("File move task stopped: {e}"),
                        ),
                        song: Some(sf),
                        existing_in_target: vec![],
                        discovery_track_id: None,
                    }),
                });
                continue;
            }
        }

        info!(
            "Moved {} -> {} ({})",
            plan.source_file.display(),
            plan.target_file.display(),
            origin.as_str()
        );

        // Move sidecars & copy/move album cover art, then prune empty source directories.
        if let (Some(src_dir), Some(dst_dir)) = (plan.source_file.parent(), plan.target_file.parent())
        {
            let sidecar_warnings = transfer_sidecars(&sidecars, src_dir, dst_dir, None);
            warnings.extend(sidecar_warnings);
            if let Err(e) =
                crate::server_fns::cleanup_empty_ancestors(src_dir, &plan.source_folder_root).await
            {
                warnings.push(format!("Could not prune empty source folders: {e}"));
            }
        }

        // Update source and destination beets DBs through ImportQueue (ordered by path).
        let forget_outcome = queue.forget(&plan.source_folder_root, &plan.source_file).await;
        if let Some(note) = forget_note(&forget_outcome, &plan.source_file, &plan.source_folder_root)
        {
            if !matches!(forget_outcome, ForgetOutcome::NotInLibrary) {
                warnings.push(note);
            }
        }
        match queue.register_in_place(&target_root, &plan.target_file).await {
            RegisterOutcome::Registered | RegisterOutcome::AlreadyInLibrary => {}
            RegisterOutcome::Failed(e) => {
                warnings.push(format!(
                    "Moved the file, but could not register it in the destination folder's beets library: {e}"
                ));
            }
        }

        moved.push(MovedOnDisk {
            idx,
            plan,
            replaced,
            warnings,
        });
    }

    // Trigger one full-library Navidrome scan covering all moved tracks and verify song IDs.
    let scan_outcome = if moved.is_empty() {
        None
    } else {
        Some(
            trigger_and_wait_scan(
                guard.db,
                navidrome,
                &user,
                &own_navi,
                Stage::Move,
                SCAN_WAIT_MAX,
            )
            .await,
        )
    };

    let scan_status = scan_outcome.as_ref().map(|o| match o {
        ScanWaitOutcome::Completed => "completed".to_string(),
        ScanWaitOutcome::NoScanAccount => "no_scan_account".to_string(),
        ScanWaitOutcome::TimedOut => "timed_out".to_string(),
        ScanWaitOutcome::Failed(_) => "failed".to_string(),
    });

    // Track which split albums we already checked/restored so we only do it once per album.
    let mut restored_albums: HashMap<String, bool> = HashMap::new();
    let target_owner_navi = if req.admin_access && target_folder.user_id != ctx.user_id {
        navidrome
            .navidrome_of(&UserId::new(&target_folder.user_id))
            .await
            .ok()
    } else {
        None
    };

    for mut m in moved {
        let (status, new_song_id, song_id_preserved, updated_song) = match &scan_outcome {
            Some(ScanWaitOutcome::Completed) => {
                let verify_song = match own_navi.get_native_song(&m.plan.song.id).await {
                    Ok(Some(found)) if !found.missing => Some(found),
                    _ => match &target_owner_navi {
                        Some(other_navi) => other_navi
                            .get_native_song(&m.plan.song.id)
                            .await
                            .ok()
                            .flatten()
                            .filter(|f| !f.missing),
                        None => None,
                    },
                };
                match verify_song {
                    Some(found) => {
                        let _ = paths::learn_in(guard.db, std::slice::from_ref(&found), &m.plan.target_file).await;
                        let matches_dst = paths::names_file(
                            guard.db,
                            &found,
                            guard.music_path,
                            &m.plan.target_file,
                        )
                        .await
                        .unwrap_or(false)
                            || found.library_id != m.plan.song.library_id
                            || found.path != m.plan.song.path;
                        if matches_dst {
                            ("moved".to_string(), Some(found.id.clone()), Some(true), found)
                        } else {
                            m.warnings.push(
                                "Navidrome completed a scan, but still reports this song at its old location; it may update on the next scan".into(),
                            );
                            ("moved_pending_scan".to_string(), Some(found.id.clone()), None, found)
                        }
                    }
                    None => {
                        // Look up by destination path to see if Navidrome assigned a new ID.
                        let rel = m
                            .plan
                            .target_file
                            .strip_prefix(&target_root)
                            .unwrap_or(&m.plan.target_file);
                        let item = ImportedItem {
                            path: rel.to_path_buf(),
                            recording_mbid: m.plan.song.mbz_recording_id.clone(),
                            artist: m.plan.song.artist.clone(),
                            title: m.plan.song.title.clone(),
                            album: m.plan.song.album.clone(),
                            beets_id: None,
                            release_mbid: None,
                            match_per_mille: None,
                        };
                        let found_new = match library_songs(&own_navi, &target_root, &item)
                            .await
                            .ok()
                            .and_then(|v| v.into_iter().next())
                        {
                            Some(s) => Some(s),
                            None => match &target_owner_navi {
                                Some(other_navi) => library_songs(other_navi, &target_root, &item)
                                    .await
                                    .ok()
                                    .and_then(|v| v.into_iter().next()),
                                None => None,
                            },
                        };
                        if let Some(new_song) = found_new {
                            let _ = paths::learn_in(
                                guard.db,
                                std::slice::from_ref(&new_song),
                                &m.plan.target_file,
                            )
                            .await;
                            m.warnings.push(format!(
                                "Navidrome did not preserve song_id {} across the move and indexed the file as {}; ratings or playlist entries may need checking",
                                m.plan.song.id, new_song.id
                            ));
                            (
                                "moved_id_changed".to_string(),
                                Some(new_song.id.clone()),
                                Some(false),
                                new_song,
                            )
                        } else {
                            m.warnings.push(
                                "Navidrome scanned after the move and does not list the file at the destination yet; check that the target folder is inside a Navidrome library".into(),
                            );
                            (
                                "moved_pending_scan".to_string(),
                                None,
                                None,
                                m.plan.song.clone(),
                            )
                        }
                    }
                }
            }
            Some(ScanWaitOutcome::NoScanAccount) => {
                m.warnings.push(
                    "File moved and beets updated, but no linked Navidrome account is an admin to start a scan; Navidrome will pick up the move at its next scan".into(),
                );
                (
                    "moved_pending_scan".to_string(),
                    None,
                    None,
                    m.plan.song.clone(),
                )
            }
            Some(ScanWaitOutcome::TimedOut) => {
                m.warnings.push(
                    "File moved and beets updated; Navidrome scan is still running so song_id preservation is not yet verified".into(),
                );
                (
                    "moved_pending_scan".to_string(),
                    None,
                    None,
                    m.plan.song.clone(),
                )
            }
            Some(ScanWaitOutcome::Failed(f)) => {
                m.warnings.push(format!(
                    "File moved and beets updated, but Navidrome scan failed: {}",
                    f.message
                ));
                (
                    "moved_pending_scan".to_string(),
                    None,
                    None,
                    m.plan.song.clone(),
                )
            }
            None => (
                "moved_pending_scan".to_string(),
                None,
                None,
                m.plan.song.clone(),
            ),
        };

        // Restore reader's album rating/star on old_album_id if Navidrome's ReassignAnnotation
        // stole it during a partial-album cross-library move.
        let mut album_annotation_restored = false;
        if matches!(scan_outcome, Some(ScanWaitOutcome::Completed)) {
            if let Some((old_album_id, old_rating, old_starred)) = &m.plan.album_annotation {
                let restore_key = format!("{}:{old_album_id}", m.plan.reader_user);
                if let Some(&already) = restored_albums.get(&restore_key) {
                    album_annotation_restored = already;
                } else {
                    let mut restored = false;
                    let reader_navi = if m.plan.reader_user == user {
                        Some(own_navi.clone())
                    } else {
                        navidrome.navidrome_of(&m.plan.reader_user).await.ok()
                    };
                    if let Some(ref navi_client) = reader_navi {
                        if let Ok(Some(current)) = navi_client.get_album(old_album_id).await {
                            if *old_rating > 0 && current.user_rating.unwrap_or(0) == 0 {
                                if let Err(e) = navi_client.set_rating(old_album_id, *old_rating).await {
                                    m.warnings.push(format!(
                                        "Could not restore your {old_rating}-star rating on the original album: {e}"
                                    ));
                                } else {
                                    restored = true;
                                }
                            }
                            if *old_starred && current.starred.is_none() {
                                if let Err(e) = navi_client
                                    .set_star(StarTarget::Album(old_album_id), true)
                                    .await
                                {
                                    m.warnings.push(format!(
                                        "Could not restore your star on the original album: {e}"
                                    ));
                                } else {
                                    restored = true;
                                }
                            }
                        }
                    }
                    restored_albums.insert(restore_key, restored);
                    album_annotation_restored = restored;
                }
            }
        }

        // Persist move audit record.
        let record = MoveRecord {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: ctx.user_id.clone(),
            song_id: m.plan.song.id.clone(),
            new_song_id: new_song_id.clone(),
            title: m.plan.song.title.clone(),
            artist: m.plan.song.artist.clone(),
            album: m.plan.song.album.clone(),
            source_folder_id: m.plan.source_folder_ref.folder_id.clone(),
            source_path: m.plan.source_file.to_string_lossy().into_owned(),
            target_folder_id: target_folder.id.clone(),
            target_path: m.plan.target_file.to_string_lossy().into_owned(),
            origin: origin.as_str().to_string(),
            client: app_host.clone(),
            status: status.clone(),
            warnings: m.warnings.clone(),
        };
        if let Err(e) = MoveHistoryRow::insert_in(guard.db, &record).await {
            warn!("Could not record move history for {}: {e}", m.plan.song.id);
            m.warnings
                .push(format!("Move succeeded but history could not be saved: {e}"));
        }

        let mut sf = SongFacts::from(&updated_song);
        sf.folder = Some(target_ref.clone());

        entries[m.idx] = Some(MoveEntry {
            song_id: m.plan.song.id.clone(),
            outcome: Ok(MoveItemSuccess {
                song_id: m.plan.song.id,
                new_song_id,
                song_id_preserved,
                status,
                song: sf,
                source_folder: m.plan.source_folder_ref,
                source_file: m.plan.source_file.to_string_lossy().into_owned(),
                target_folder: target_ref.clone(),
                target_file: m.plan.target_file.to_string_lossy().into_owned(),
                crosses_navidrome_library: m.plan.crosses_navidrome_library,
                album_split: m.plan.album_split,
                album_annotation_restored,
                replaced: m.replaced,
                warnings: m.warnings,
            }),
        });
    }

    Ok(MoveReport {
        dry_run: false,
        target_folder: target_summary,
        album_id,
        scan_status,
        entries: entries.into_iter().flatten().collect(),
    })
}

fn find_user_folder<'a>(rows: &'a PolicyRows, user_id: &str, canonical: &Path) -> Option<&'a Folder> {
    rows.folders.iter().find_map(|r| {
        if r.folder.user_id != user_id {
            return None;
        }
        canonical_folder_path(Path::new(&r.folder.path))
            .ok()
            .filter(|c| c == canonical)
            .map(|_| &r.folder)
    })
}

/// Runs the delete guard's multi-user veto checks (rules 3 & 4) on a move that could hide
/// `song` from another listener. `reader` is the account that can see `song` in Navidrome
/// (usually `actor`, or another linked account during an admin cross-user move).
async fn check_multi_user_veto(
    guard: &Guard<'_>,
    actor: &UserId,
    actor_name: &str,
    reader: &UserId,
    song: &NativeSong,
    path: &Path,
) -> Result<(), Failure> {
    guard
        .check_move_veto(actor, actor_name, reader, song, path)
        .await
}

/// Finds Navidrome songs in `target_root` that either occupy `target_file` or match `song`
/// logically (same recording MBID or same normalized artist & title).
async fn find_target_duplicates(
    guard: &Guard<'_>,
    actor: &UserId,
    song: &NativeSong,
    target_root: &Path,
    target_file: &Path,
) -> Result<Vec<NativeSong>, Failure> {
    let item = ImportedItem {
        path: PathBuf::from(&song.path),
        recording_mbid: song.mbz_recording_id.clone(),
        artist: song.artist.clone(),
        title: song.title.clone(),
        album: song.album.clone(),
        beets_id: None,
        release_mbid: None,
        match_per_mille: None,
    };
    let candidates = guard
        .listeners
        .songs_for_item(actor, &item)
        .await
        .map_err(|e| e.failure(Stage::Move, e.to_string()))?
        .unwrap_or_default();
    let same = |a: &str, b: &str| normalize_for_matching(a) == normalize_for_matching(b);
    let rec_mbid = song.mbz_recording_id.as_deref().filter(|s| !s.is_empty());

    let mut found = Vec::new();
    for cand in candidates {
        if cand.id == song.id || cand.missing {
            continue;
        }
        let Ok(root) = paths::root_in(guard.db, &cand, guard.music_path).await else {
            continue;
        };
        let resolved_root = canonical_folder_path(&root).unwrap_or(root);
        let cand_path = resolved_root.join(&cand.path);
        let in_target = cand_path.starts_with(target_root)
            || tokio::fs::canonicalize(&cand_path)
                .await
                .is_ok_and(|c| c.starts_with(target_root));
        if !in_target {
            continue;
        }
        let same_file = cand_path == target_file
            || tokio::fs::canonicalize(&cand_path)
                .await
                .is_ok_and(|c| c == target_file);
        let same_recording = rec_mbid.is_some()
            && cand
                .mbz_recording_id
                .as_deref()
                .filter(|s| !s.is_empty())
                == rec_mbid;
        let same_name = same(&cand.artist, &song.artist) && same(&cand.title, &song.title);
        if same_file || same_recording || same_name {
            found.push(cand);
        }
    }
    Ok(found)
}
