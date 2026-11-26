//! Deep inspection of library tracks: folder ownership, local path, Navidrome library,
//! beets metadata, album peers, sidecar files, playlists, discovery status, and multi-user
//! visibility (without naming other listeners).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;
use shared::failure::{cut_json, Failure, FailureCode, RetryAdvice, Stage, TEXT_MAX_CHARS};
use soulbeet::navidrome::models::SubsonicAlbum;
use soulbeet::BeetsTrackInfo;
use sqlx::SqlitePool;

use super::find_sidecars;
use crate::auth::UserCtx;
use crate::core::database_failure;
use crate::core::delete_guard::{paths, rules, Listeners};
use crate::core::library::{known_roots_in, listing, FolderRef, Listing, SongFacts};
use crate::folder_policy::PolicyRows;
use crate::import_queue::ImportQueue;
use crate::models::discovery_playlist::DiscoveryTrackRow;
use crate::models::user::User;

#[derive(Debug, Clone, Serialize)]
pub struct TrackPlaylistInfo {
    pub playlist_id: String,
    pub name: String,
    pub smart: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrackAlbumInfo {
    pub album_id: String,
    pub name: String,
    pub song_count: usize,
    pub user_rating: u8,
    pub starred: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrackDiscoveryInfo {
    pub track_id: String,
    pub profile: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TrackFolderInfo {
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
pub(crate) struct InspectedTrack {
    pub song_id: String,
    pub song: SongFacts,
    pub navidrome_library_id: i64,
    pub file: Option<String>,
    pub file_error: Option<String>,
    pub folder: Option<TrackFolderInfo>,
    pub beets: Option<BeetsTrackInfo>,
    pub beets_error: Option<String>,
    pub album: Option<TrackAlbumInfo>,
    pub playlists: Vec<TrackPlaylistInfo>,
    pub other_listeners_in_playlists: bool,
    pub visible_to_other_users: bool,
    pub sidecars: Vec<String>,
    pub album_art: Vec<String>,
    pub pending_discovery: Option<TrackDiscoveryInfo>,
}

#[derive(Debug)]
pub(crate) struct InspectEntry {
    pub song_id: String,
    pub outcome: Result<InspectedTrack, Failure>,
}

/// Inspects up to `SONGS_PER_CALL` songs for `ctx`. When `admin_access` is true, falls back to
/// reading songs via other linked users' Navidrome accounts and resolves folders across all
/// registered users with owner metadata.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn inspect_tracks(
    db: &SqlitePool,
    listeners: &dyn Listeners,
    queue: &ImportQueue,
    music_path: Option<&Path>,
    ctx: &UserCtx,
    song_ids: &[String],
    admin_access: bool,
) -> Result<Vec<InspectEntry>, Failure> {
    let user = ctx.id();
    let rows = PolicyRows::load_in(db)
        .await
        .map_err(|e| database_failure(Stage::LibraryCheck, e))?;
    let (policy_folders, _) = rows.inputs();
    let known_roots = known_roots_in(db, music_path).await?;
    let library_roots: Vec<PathBuf> = known_roots.iter().map(|(_, r)| r.clone()).collect();
    let pending = DiscoveryTrackRow::pending_for_user_in(db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::LibraryCheck, e))?;
    let linked_users = User::linked_to_navidrome_in(db)
        .await
        .map_err(|e| database_failure(Stage::LibraryCheck, e))?;
    let other_users: Vec<crate::auth::UserId> = linked_users
        .into_iter()
        .filter(|u| u.id != ctx.user_id)
        .map(|u| crate::auth::UserId::new(&u.id))
        .collect();

    let mut album_cache: HashMap<String, Option<SubsonicAlbum>> = HashMap::new();
    let mut owner_pending_cache: HashMap<String, Vec<shared::navidrome::DiscoveryTrack>> =
        HashMap::new();
    let mut results = Vec::with_capacity(song_ids.len());

    for song_id in song_ids {
        let mut reader_user = user.clone();
        let native = match listeners.native_song(&user, song_id).await {
            Ok(Some(song)) => song,
            Ok(None) if admin_access => {
                let mut found_other = None;
                for other in &other_users {
                    if let Ok(Some(song)) = listeners.native_song(other, song_id).await {
                        reader_user = other.clone();
                        found_other = Some(song);
                        break;
                    }
                }
                match found_other {
                    Some(song) => song,
                    None => {
                        results.push(InspectEntry {
                            song_id: song_id.clone(),
                            outcome: Err(Failure::new(
                                FailureCode::UnknownHandle,
                                Stage::LibraryCheck,
                                RetryAdvice::NotRetryable,
                                "Navidrome has no song with this id that any linked account can see",
                            )),
                        });
                        continue;
                    }
                }
            }
            Ok(None) => {
                results.push(InspectEntry {
                    song_id: song_id.clone(),
                    outcome: Err(Failure::new(
                        FailureCode::UnknownHandle,
                        Stage::LibraryCheck,
                        RetryAdvice::NotRetryable,
                        "Navidrome has no song with this id that this account can see",
                    )),
                });
                continue;
            }
            Err(e) => {
                results.push(InspectEntry {
                    song_id: song_id.clone(),
                    outcome: Err(e.failure(Stage::LibraryCheck, e.to_string())),
                });
                continue;
            }
        };

        let (local_path, file_error) =
            match paths::local_file_learning(db, &native, music_path, &reader_user).await {
                Ok((path, _)) => (Some(path), None),
                Err(f) => (None, Some(f.message)),
            };

        let mut source_owner_id: Option<String> = None;
        let folder_info = local_path.as_deref().and_then(|path| {
            if admin_access {
                let (pf, owned_root) =
                    rules::deepest_registered_folder(path, &policy_folders, &user).ok()?;
                let owned_row = rows.folders.iter().find(|r| r.folder.id == pf.id)?;
                source_owner_id = Some(owned_row.folder.user_id.clone());
                let is_own = owned_row.folder.user_id == ctx.user_id;
                Some(TrackFolderInfo {
                    folder_id: owned_row.folder.id.clone(),
                    name: cut_json(owned_row.folder.name.clone(), TEXT_MAX_CHARS),
                    path: owned_root.to_string_lossy().into_owned(),
                    shared: owned_row.folder.shared,
                    in_navidrome: listing(owned_root, &library_roots),
                    owner: Some(cut_json(owned_row.owner.clone(), TEXT_MAX_CHARS)),
                    is_own: Some(is_own),
                })
            } else {
                let owned_root = rules::owning_folders(path, &policy_folders, &user).ok()?;
                let owned_row = rows.folders.iter().find(|r| {
                    r.folder.user_id == ctx.user_id
                        && crate::folder_policy::canonical_folder_path(Path::new(&r.folder.path))
                            .is_ok_and(|c| c == owned_root)
                })?;
                Some(TrackFolderInfo {
                    folder_id: owned_row.folder.id.clone(),
                    name: cut_json(owned_row.folder.name.clone(), TEXT_MAX_CHARS),
                    path: owned_root.to_string_lossy().into_owned(),
                    shared: owned_row.folder.shared,
                    in_navidrome: listing(owned_root, &library_roots),
                    owner: None,
                    is_own: None,
                })
            }
        });

        let (beets, beets_error) = match (&local_path, &folder_info) {
            (Some(file), Some(folder)) => {
                match queue.inspect_file(Path::new(&folder.path), file).await {
                    Ok(info) => (info, None),
                    Err(e) => (None, Some(e)),
                }
            }
            _ => (None, None),
        };

        let album_info = if native.album_id.is_empty() {
            None
        } else {
            if !album_cache.contains_key(&native.album_id) {
                let fetched = listeners
                    .album(&reader_user, &native.album_id)
                    .await
                    .ok()
                    .flatten();
                album_cache.insert(native.album_id.clone(), fetched);
            }
            album_cache
                .get(&native.album_id)
                .and_then(|a| a.as_ref())
                .map(|a| TrackAlbumInfo {
                    album_id: a.id.clone(),
                    name: cut_json(a.display_name().to_string(), TEXT_MAX_CHARS),
                    song_count: a.song.len(),
                    user_rating: a.user_rating.unwrap_or(0),
                    starred: a.starred.is_some(),
                })
        };

        let (playlists, mut other_listeners_in_playlists) =
            match listeners.song_playlists(&user, song_id).await {
                Ok(pls) => {
                    let other_static = pls
                        .iter()
                        .any(|p| !p.is_smart() && !p.owner_name.eq_ignore_ascii_case(&ctx.username));
                    let own: Vec<TrackPlaylistInfo> = pls
                        .into_iter()
                        .filter(|p| p.owner_name.eq_ignore_ascii_case(&ctx.username))
                        .map(|p| TrackPlaylistInfo {
                            playlist_id: p.id.clone(),
                            name: cut_json(p.name.clone(), TEXT_MAX_CHARS),
                            smart: p.is_smart(),
                        })
                        .collect();
                    (own, other_static)
                }
                Err(_) => (vec![], false),
            };

        let mut visible_to_other_users = false;
        for other in &other_users {
            if listeners.song(other, song_id).await.ok().flatten().is_some() {
                visible_to_other_users = true;
            }
            if !other_listeners_in_playlists {
                if let Ok(pls) = listeners.song_playlists(other, song_id).await {
                    if pls
                        .iter()
                        .any(|p| !p.is_smart() && !p.owner_name.eq_ignore_ascii_case(&ctx.username))
                    {
                        other_listeners_in_playlists = true;
                    }
                }
            }
        }

        let (sidecars, album_art) = match &local_path {
            Some(path) => {
                let s = find_sidecars(path);
                let names = |list: Vec<PathBuf>| -> Vec<String> {
                    list.into_iter()
                        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                        .collect()
                };
                (names(s.track_files), names(s.album_art))
            }
            None => (vec![], vec![]),
        };

        let mut pending_discovery = pending
            .iter()
            .find(|t| {
                t.song_id.as_deref() == Some(song_id.as_str())
                    || local_path.as_ref().is_some_and(|local| {
                        std::fs::canonicalize(&t.path).is_ok_and(|c| c == *local)
                    })
            })
            .map(|t| TrackDiscoveryInfo {
                track_id: t.id.clone(),
                profile: t.profile.clone(),
            });
        if pending_discovery.is_none() && admin_access {
            if let Some(ref owner_id) = source_owner_id {
                if owner_id != &ctx.user_id {
                    if !owner_pending_cache.contains_key(owner_id) {
                        let owner_pending = DiscoveryTrackRow::pending_for_user_in(db, owner_id)
                            .await
                            .unwrap_or_default();
                        owner_pending_cache.insert(owner_id.clone(), owner_pending);
                    }
                    if let Some(owner_list) = owner_pending_cache.get(owner_id) {
                        pending_discovery = owner_list
                            .iter()
                            .find(|t| {
                                t.song_id.as_deref() == Some(song_id.as_str())
                                    || local_path.as_ref().is_some_and(|local| {
                                        std::fs::canonicalize(&t.path).is_ok_and(|c| c == *local)
                                    })
                            })
                            .map(|t| TrackDiscoveryInfo {
                                track_id: t.id.clone(),
                                profile: t.profile.clone(),
                            });
                    }
                }
            }
        }

        let mut song_facts = SongFacts::from(&native);
        song_facts.folder = folder_info.as_ref().map(|f| FolderRef {
            folder_id: f.folder_id.clone(),
            name: f.name.clone(),
            shared: f.shared,
            owner: f.owner.clone(),
            is_own: f.is_own,
        });

        results.push(InspectEntry {
            song_id: song_id.clone(),
            outcome: Ok(InspectedTrack {
                song_id: song_id.clone(),
                song: song_facts,
                navidrome_library_id: native.library_id,
                file: local_path.map(|p| p.to_string_lossy().into_owned()),
                file_error,
                folder: folder_info,
                beets,
                beets_error,
                album: album_info,
                playlists,
                other_listeners_in_playlists,
                visible_to_other_users,
                sidecars,
                album_art,
                pending_discovery,
            }),
        });
    }

    Ok(results)
}
