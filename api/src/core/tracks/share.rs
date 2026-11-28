//! User-to-user track and album sharing via filesystem hardlinks (with cross-FS
//! reflink/copy fallback), beets registration, Navidrome scan, and audit history.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage};
use shared::recommendation::normalize_for_matching;
use soulbeet::navidrome::models::NativeSong;
use soulbeet::ImportedItem;
use sqlx::SqlitePool;
use tracing::{info, warn};

use super::link::{link_or_copy_file, predict_link_mode, share_sidecars};
use super::{
    find_sidecars, trigger_and_wait_scan, wait_for_idle_scan, ScanWaitOutcome, TrackSelection,
    ALBUM_MAX_TRACKS, IDLE_SCAN_WAIT_MAX, SCAN_WAIT_MAX, SONGS_PER_CALL,
};
use crate::auth::{UserCtx, UserId};
use crate::core::database_failure;
use crate::core::delete_guard::{paths, rules, Guard};
use crate::core::index::NavidromeOf;
use crate::folder_policy::{canonical_folder_path, PolicyRows};
use crate::import_queue::ImportQueue;
use crate::models::discovery_playlist::DiscoveryTrackRow;
use crate::models::folder::Folder;
use crate::models::track_share::{LinkMode, NewTrackShare, ShareOrigin, TrackShareRow};
use crate::models::user::User;
use crate::models::user_settings::UserSettings;

/// Public recipient summary safe to return to non-admin users.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareRecipient {
    pub user_id: String,
    pub username: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareItemStatus {
    Shared,
    WouldShare,
    AlreadyInLibrary,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShareResultEntry {
    pub song_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub recipient_user_id: String,
    pub recipient_username: String,
    pub status: ShareItemStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_mode: Option<LinkMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_song_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShareTracksReport {
    pub batch_id: String,
    pub dry_run: bool,
    pub shared_count: usize,
    pub already_in_library_count: usize,
    pub failed_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_status: Option<String>,
    pub entries: Vec<ShareResultEntry>,
}

pub struct ShareTracksRequest {
    pub selection: TrackSelection,
    pub recipient_user_ids: Vec<String>,
    pub dry_run: bool,
    pub origin: ShareOrigin,
}

/// Lists Soulbeet users (excluding `actor`) who accept incoming shares and have
/// a configured library folder. Never leaks admin metadata or folder paths.
pub async fn list_recipients(ctx: &UserCtx) -> Result<Vec<ShareRecipient>, Failure> {
    list_recipients_in(&crate::db::DB, ctx).await
}

pub(crate) async fn list_recipients_in(
    db: &SqlitePool,
    ctx: &UserCtx,
) -> Result<Vec<ShareRecipient>, Failure> {
    let users = User::get_all_in(db)
        .await
        .map_err(|e| database_failure(Stage::Share, e))?;
    let mut recipients = Vec::new();
    for u in users {
        if u.id == ctx.user_id {
            continue;
        }
        let settings = UserSettings::get_in(db, &u.id)
            .await
            .map_err(|e| database_failure(Stage::Share, e))?;
        if !settings.accept_shares {
            continue;
        }
        let folder = Folder::get_for_user_in(db, &u.id)
            .await
            .map_err(|e| database_failure(Stage::Share, e))?;
        if folder.is_some() {
            recipients.push(ShareRecipient {
                user_id: u.id,
                username: u.username,
            });
        }
    }
    recipients.sort_by_key(|a| a.username.to_lowercase());
    Ok(recipients)
}

/// Resolves usernames (case-insensitive) or user IDs to distinct Soulbeet user IDs.
pub(crate) async fn resolve_recipient_ids_in(
    db: &SqlitePool,
    ids: &[String],
    usernames: &[String],
) -> Result<Vec<String>, Failure> {
    let users = User::get_all_in(db)
        .await
        .map_err(|e| database_failure(Stage::Share, e))?;
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for id in ids {
        let trimmed = id.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(u) = users.iter().find(|u| u.id == trimmed) else {
            return Err(Failure::new(
                FailureCode::UnknownHandle,
                Stage::Share,
                RetryAdvice::NotRetryable,
                format!("Unknown recipient user id '{trimmed}'"),
            ));
        };
        if seen.insert(u.id.clone()) {
            out.push(u.id.clone());
        }
    }

    for name in usernames {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(u) = users
            .iter()
            .find(|u| u.username.eq_ignore_ascii_case(trimmed))
        else {
            return Err(Failure::new(
                FailureCode::UnknownHandle,
                Stage::Share,
                RetryAdvice::NotRetryable,
                format!("Unknown recipient username '{trimmed}'"),
            ));
        };
        if seen.insert(u.id.clone()) {
            out.push(u.id.clone());
        }
    }

    if out.is_empty() {
        return Err(Failure::new(
            FailureCode::InvalidInput,
            Stage::Share,
            RetryAdvice::NotRetryable,
            "Select at least one recipient user to share with",
        ));
    }

    Ok(out)
}

struct CheckedRecipient {
    user: User,
    target_root: PathBuf,
}

struct CheckedTrack {
    song: NativeSong,
    source_file: PathBuf,
    rel_path: PathBuf,
}

/// Shares tracks or an entire album from `ctx`'s library into one or more
/// recipient users' libraries via hardlinks (with cross-FS reflink/copy fallback).
pub(crate) async fn share_tracks(
    guard: &Guard<'_>,
    navidrome: &dyn NavidromeOf,
    queue: &ImportQueue,
    ctx: &UserCtx,
    req: ShareTracksRequest,
) -> Result<ShareTracksReport, Failure> {
    if req.recipient_user_ids.is_empty() {
        return Err(Failure::new(
            FailureCode::InvalidInput,
            Stage::Share,
            RetryAdvice::NotRetryable,
            "Select at least one recipient user to share with",
        ));
    }

    let sender = UserId::new(&ctx.user_id);
    let batch_id = uuid::Uuid::new_v4().to_string();

    // 1. Validate recipients (consent + configured library folder + not self).
    let mut recipients: Vec<CheckedRecipient> = Vec::new();
    let mut seen_recipients = HashSet::new();
    for raw_id in &req.recipient_user_ids {
        if !seen_recipients.insert(raw_id.clone()) {
            continue;
        }
        if raw_id == &ctx.user_id {
            return Err(Failure::new(
                FailureCode::ShareSelfDisallowed,
                Stage::Share,
                RetryAdvice::NotRetryable,
                "You cannot share tracks to your own account",
            ));
        }
        let user = User::find_by_id_in(guard.db, raw_id)
            .await
            .map_err(|e| database_failure(Stage::Share, e))?
            .ok_or_else(|| {
                Failure::new(
                    FailureCode::UnknownHandle,
                    Stage::Share,
                    RetryAdvice::NotRetryable,
                    format!("Recipient user '{raw_id}' not found"),
                )
            })?;
        let settings = UserSettings::get_in(guard.db, &user.id)
            .await
            .map_err(|e| database_failure(Stage::Share, e))?;
        if !settings.accept_shares {
            return Err(Failure::new(
                FailureCode::ShareRecipientDisabled,
                Stage::Share,
                RetryAdvice::NeedsUser,
                format!("{} has disabled incoming track shares", user.username),
            ));
        }
        let folder = Folder::get_for_user_in(guard.db, &user.id)
            .await
            .map_err(|e| database_failure(Stage::Share, e))?
            .ok_or_else(|| {
                Failure::new(
                    FailureCode::ShareNoTargetFolder,
                    Stage::Share,
                    RetryAdvice::NeedsUser,
                    format!("{} does not have a music library folder configured", user.username),
                )
            })?;
        let target_root = canonical_folder_path(Path::new(&folder.path)).map_err(|e| {
            Failure::new(
                FailureCode::ShareFailed,
                Stage::Share,
                RetryAdvice::NotRetryable,
                format!(
                    "Recipient {}'s library folder path could not be resolved: {e}",
                    user.username
                ),
            )
        })?;
        recipients.push(CheckedRecipient { user, target_root });
    }

    // 2. Resolve source song IDs from selection.
    let song_ids: Vec<String> = match &req.selection {
        TrackSelection::Songs(ids) => {
            if ids.is_empty() {
                return Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Share,
                    RetryAdvice::NotRetryable,
                    "At least one song_id is required",
                ));
            }
            if ids.len() > SONGS_PER_CALL {
                return Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Share,
                    RetryAdvice::NotRetryable,
                    format!("At most {SONGS_PER_CALL} songs may be shared per call"),
                ));
            }
            let mut dedup = Vec::with_capacity(ids.len());
            let mut seen = HashSet::new();
            for id in ids {
                if seen.insert(id.clone()) {
                    dedup.push(id.clone());
                }
            }
            dedup
        }
        TrackSelection::Album(album_id) => {
            let album = guard
                .listeners
                .album(&sender, album_id)
                .await
                .map_err(|e| e.failure(Stage::Share, e.to_string()))?
                .ok_or_else(|| {
                    Failure::new(
                        FailureCode::UnknownHandle,
                        Stage::Share,
                        RetryAdvice::NotRetryable,
                        "Album not found in your Navidrome library",
                    )
                })?;
            if album.song.is_empty() {
                return Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Share,
                    RetryAdvice::NotRetryable,
                    "Album has no tracks to share",
                ));
            }
            if album.song.len() > ALBUM_MAX_TRACKS {
                return Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Share,
                    RetryAdvice::NotRetryable,
                    format!("Album exceeds maximum {ALBUM_MAX_TRACKS} tracks per share call"),
                ));
            }
            album.song.into_iter().map(|s| s.id).collect()
        }
    };

    // 3. Load sender's folder policy & pending discovery tracks.
    let rows = PolicyRows::load_in(guard.db)
        .await
        .map_err(|e| database_failure(Stage::Share, e))?;
    let (policy_folders, _) = rows.inputs();

    let pending_discovery = DiscoveryTrackRow::pending_for_user_in(guard.db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Share, e))?;
    let pending_song_ids: HashSet<String> = pending_discovery
        .iter()
        .filter_map(|t| t.song_id.clone())
        .collect();
    let pending_paths: HashSet<PathBuf> = pending_discovery
        .iter()
        .filter_map(|t| std::fs::canonicalize(&t.path).ok())
        .collect();

    let mut checked_tracks: Vec<CheckedTrack> = Vec::new();
    let mut entries: Vec<ShareResultEntry> = Vec::new();

    for song_id in &song_ids {
        let song = match guard.listeners.native_song(&sender, song_id).await {
            Ok(Some(s)) => s,
            Ok(None) => {
                for r in &recipients {
                    entries.push(failed_entry(
                        song_id,
                        "",
                        "",
                        "",
                        r,
                        Failure::new(
                            FailureCode::UnknownHandle,
                            Stage::Share,
                            RetryAdvice::NotRetryable,
                            "Song not found in your Navidrome library",
                        ),
                    ));
                }
                continue;
            }
            Err(e) => {
                let fail = e.failure(Stage::Share, e.to_string());
                for r in &recipients {
                    entries.push(failed_entry(song_id, "", "", "", r, fail.clone()));
                }
                continue;
            }
        };

        if pending_song_ids.contains(&song.id) {
            let fail = Failure::new(
                FailureCode::InvalidInput,
                Stage::Share,
                RetryAdvice::NeedsUser,
                "Pending discovery tracks must be promoted to your library before sharing",
            );
            for r in &recipients {
                entries.push(failed_entry(
                    &song.id,
                    &song.title,
                    &song.artist,
                    &song.album,
                    r,
                    fail.clone(),
                ));
            }
            continue;
        }

        let (source_file, _) = match paths::local_file_learning(
            guard.db,
            &song,
            guard.music_path,
            &sender,
        )
        .await
        {
            Ok(v) => v,
            Err(fail) => {
                for r in &recipients {
                    entries.push(failed_entry(
                        &song.id,
                        &song.title,
                        &song.artist,
                        &song.album,
                        r,
                        fail.clone(),
                    ));
                }
                continue;
            }
        };

        if pending_paths.contains(&source_file) {
            let fail = Failure::new(
                FailureCode::InvalidInput,
                Stage::Share,
                RetryAdvice::NeedsUser,
                "Pending discovery tracks must be promoted to your library before sharing",
            );
            for r in &recipients {
                entries.push(failed_entry(
                    &song.id,
                    &song.title,
                    &song.artist,
                    &song.album,
                    r,
                    fail.clone(),
                ));
            }
            continue;
        }

        let source_root = match rules::owning_folders(&source_file, &policy_folders, &sender) {
            Ok(root) => root.to_path_buf(),
            Err(fail) => {
                for r in &recipients {
                    entries.push(failed_entry(
                        &song.id,
                        &song.title,
                        &song.artist,
                        &song.album,
                        r,
                        fail.clone(),
                    ));
                }
                continue;
            }
        };

        if let Err(fail) = rules::audio_file(&source_file) {
            for r in &recipients {
                entries.push(failed_entry(
                    &song.id,
                    &song.title,
                    &song.artist,
                    &song.album,
                    r,
                    fail.clone(),
                ));
            }
            continue;
        }

        let rel_path = match source_file.strip_prefix(&source_root) {
            Ok(rel) if !rel.as_os_str().is_empty() => rel.to_path_buf(),
            _ => {
                let fail = Failure::new(
                    FailureCode::ShareFailed,
                    Stage::Share,
                    RetryAdvice::NotRetryable,
                    "Could not compute track relative path inside your library folder",
                );
                for r in &recipients {
                    entries.push(failed_entry(
                        &song.id,
                        &song.title,
                        &song.artist,
                        &song.album,
                        r,
                        fail.clone(),
                    ));
                }
                continue;
            }
        };

        checked_tracks.push(CheckedTrack {
            song,
            source_file,
            rel_path,
        });
    }

    // 4. Plan (and execute if !dry_run) each (track, recipient) pair.
    if !req.dry_run && !checked_tracks.is_empty() {
        if let Ok(own_navi) = navidrome.navidrome_of(&sender).await {
            let _ = wait_for_idle_scan(&own_navi, IDLE_SCAN_WAIT_MAX).await;
        }
    }

    struct LinkedRecord {
        entry_idx: usize,
        recipient_id: String,
        target_root: PathBuf,
        target_file: PathBuf,
        song: NativeSong,
        source_file: PathBuf,
        link_mode: LinkMode,
    }
    let mut linked: Vec<LinkedRecord> = Vec::new();

    for track in &checked_tracks {
        for r in &recipients {
            let target_file = r.target_root.join(&track.rel_path);
            let target_str = target_file.to_string_lossy().into_owned();

            // Check duplicate on disk or in recipient's Navidrome library.
            let disk_exists = tokio::fs::try_exists(&target_file).await.unwrap_or(false);
            let existing_song_id = if !disk_exists {
                find_recipient_duplicate(guard, &r.user.id, &track.song, &r.target_root).await
            } else {
                None
            };

            if disk_exists || existing_song_id.is_some() {
                entries.push(ShareResultEntry {
                    song_id: track.song.id.clone(),
                    title: track.song.title.clone(),
                    artist: track.song.artist.clone(),
                    album: track.song.album.clone(),
                    recipient_user_id: r.user.id.clone(),
                    recipient_username: r.user.username.clone(),
                    status: ShareItemStatus::AlreadyInLibrary,
                    link_mode: None,
                    target_path: Some(target_str),
                    target_song_id: existing_song_id,
                    failure: None,
                    warnings: Vec::new(),
                });
                continue;
            }

            if req.dry_run {
                let predicted = predict_link_mode(&track.source_file, &r.target_root);
                entries.push(ShareResultEntry {
                    song_id: track.song.id.clone(),
                    title: track.song.title.clone(),
                    artist: track.song.artist.clone(),
                    album: track.song.album.clone(),
                    recipient_user_id: r.user.id.clone(),
                    recipient_username: r.user.username.clone(),
                    status: ShareItemStatus::WouldShare,
                    link_mode: Some(predicted),
                    target_path: Some(target_str),
                    target_song_id: None,
                    failure: None,
                    warnings: Vec::new(),
                });
                continue;
            }

            let src_clone = track.source_file.clone();
            let dst_clone = target_file.clone();
            let link_res = tokio::task::spawn_blocking(move || -> std::io::Result<(LinkMode, Vec<String>)> {
                let mode = link_or_copy_file(&src_clone, &dst_clone)?;
                let sidecars = find_sidecars(&src_clone);
                let warnings = match dst_clone.parent() {
                    Some(dst_dir) => share_sidecars(&sidecars, dst_dir, dst_clone.file_stem()),
                    None => Vec::new(),
                };
                Ok((mode, warnings))
            })
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))
            .and_then(|r| r);

            match link_res {
                Ok((mode, mut warnings)) => {
                    match queue.register_in_place(&r.target_root, &target_file).await {
                        soulbeet::RegisterOutcome::Registered
                        | soulbeet::RegisterOutcome::AlreadyInLibrary => {}
                        soulbeet::RegisterOutcome::Failed(e) => {
                            warnings.push(format!("Shared file, but beets registration warned: {e}"));
                        }
                    }
                    let entry_idx = entries.len();
                    entries.push(ShareResultEntry {
                        song_id: track.song.id.clone(),
                        title: track.song.title.clone(),
                        artist: track.song.artist.clone(),
                        album: track.song.album.clone(),
                        recipient_user_id: r.user.id.clone(),
                        recipient_username: r.user.username.clone(),
                        status: ShareItemStatus::Shared,
                        link_mode: Some(mode),
                        target_path: Some(target_str),
                        target_song_id: None,
                        failure: None,
                        warnings,
                    });
                    linked.push(LinkedRecord {
                        entry_idx,
                        recipient_id: r.user.id.clone(),
                        target_root: r.target_root.clone(),
                        target_file,
                        song: track.song.clone(),
                        source_file: track.source_file.clone(),
                        link_mode: mode,
                    });
                }
                Err(e) => {
                    let code = if e.kind() == std::io::ErrorKind::AlreadyExists {
                        FailureCode::ShareTargetExists
                    } else {
                        FailureCode::ShareFailed
                    };
                    entries.push(failed_entry(
                        &track.song.id,
                        &track.song.title,
                        &track.song.artist,
                        &track.song.album,
                        r,
                        Failure::new(
                            code,
                            Stage::Share,
                            RetryAdvice::NotRetryable,
                            format!("Could not share track to {}: {e}", r.user.username),
                        )
                        .with_cause(Service::Filesystem, None, e.to_string()),
                    ));
                }
            }
        }
    }

    // 5. Trigger one Navidrome scan and learn recipient library roots + target song IDs.
    let scan_status = if !req.dry_run && !linked.is_empty() {
        let outcome = match navidrome.navidrome_of(&sender).await {
            Ok(own_navi) => {
                trigger_and_wait_scan(
                    guard.db,
                    navidrome,
                    &sender,
                    &own_navi,
                    Stage::Share,
                    SCAN_WAIT_MAX,
                )
                .await
            }
            Err(_) => ScanWaitOutcome::NoScanAccount,
        };
        let label = match &outcome {
            ScanWaitOutcome::Completed => "completed",
            ScanWaitOutcome::NoScanAccount => "no_scan_account",
            ScanWaitOutcome::TimedOut => "timed_out",
            ScanWaitOutcome::Failed(_) => "failed",
        }
        .to_string();

        // Learn recipient Navidrome library roots and record target song IDs in audit rows.
        for rec in &linked {
            let target_song_id = if outcome == ScanWaitOutcome::Completed {
                learn_recipient_song(guard, &rec.recipient_id, &rec.song, &rec.target_file).await
            } else {
                None
            };
            entries[rec.entry_idx].target_song_id = target_song_id.clone();
            let _ = TrackShareRow::insert_in(
                guard.db,
                &NewTrackShare {
                    batch_id: &batch_id,
                    sender_user_id: &ctx.user_id,
                    recipient_user_id: &rec.recipient_id,
                    source_song_id: &rec.song.id,
                    target_song_id: target_song_id.as_deref(),
                    source_path: &rec.source_file.to_string_lossy(),
                    target_path: &rec.target_file.to_string_lossy(),
                    artist: &rec.song.artist,
                    album: &rec.song.album,
                    title: &rec.song.title,
                    link_mode: rec.link_mode,
                    origin: req.origin,
                },
            )
            .await;
            info!(
                "Shared '{}' to {} via {} ({})",
                rec.song.title,
                entries[rec.entry_idx].recipient_username,
                rec.link_mode.as_str(),
                rec.target_root.display()
            );
        }

        Some(label)
    } else {
        None
    };

    let shared_count = entries
        .iter()
        .filter(|e| matches!(e.status, ShareItemStatus::Shared | ShareItemStatus::WouldShare))
        .count();
    let already_in_library_count = entries
        .iter()
        .filter(|e| e.status == ShareItemStatus::AlreadyInLibrary)
        .count();
    let failed_count = entries
        .iter()
        .filter(|e| e.status == ShareItemStatus::Failed)
        .count();

    Ok(ShareTracksReport {
        batch_id,
        dry_run: req.dry_run,
        shared_count,
        already_in_library_count,
        failed_count,
        scan_status,
        entries,
    })
}

fn failed_entry(
    song_id: &str,
    title: &str,
    artist: &str,
    album: &str,
    r: &CheckedRecipient,
    failure: Failure,
) -> ShareResultEntry {
    ShareResultEntry {
        song_id: song_id.to_string(),
        title: title.to_string(),
        artist: artist.to_string(),
        album: album.to_string(),
        recipient_user_id: r.user.id.clone(),
        recipient_username: r.user.username.clone(),
        status: ShareItemStatus::Failed,
        link_mode: None,
        target_path: None,
        target_song_id: None,
        failure: Some(failure),
        warnings: Vec::new(),
    }
}

fn item_for_song(song: &NativeSong, path: &Path) -> ImportedItem {
    ImportedItem {
        path: path.to_path_buf(),
        recording_mbid: song.mbz_recording_id.clone(),
        artist: song.artist.clone(),
        title: song.title.clone(),
        album: song.album.clone(),
        beets_id: None,
        release_mbid: None,
        match_per_mille: None,
    }
}

/// Checks if the recipient already has a matching song in their Navidrome view
/// under `target_root` (by recording MBID or normalized artist + title).
async fn find_recipient_duplicate(
    guard: &Guard<'_>,
    recipient_id: &str,
    song: &NativeSong,
    target_root: &Path,
) -> Option<String> {
    let recipient = UserId::new(recipient_id);
    let item = item_for_song(song, Path::new(&song.path));
    let Ok(Some(candidates)) = guard.listeners.songs_for_item(&recipient, &item).await else {
        return None;
    };
    let want_mbid = song.mbz_recording_id.as_deref().filter(|s| !s.is_empty());
    let want_artist = normalize_for_matching(&song.artist);
    let want_title = normalize_for_matching(&song.title);

    for cand in candidates {
        let same_mbid = want_mbid.is_some()
            && cand
                .mbz_recording_id
                .as_deref()
                .filter(|s| !s.is_empty())
                == want_mbid;
        let same_name = normalize_for_matching(&cand.artist) == want_artist
            && normalize_for_matching(&cand.title) == want_title;
        if !same_mbid && !same_name {
            continue;
        }
        if let Ok((local, _)) =
            paths::local_file_learning(guard.db, &cand, guard.music_path, &recipient).await
        {
            if local.starts_with(target_root) {
                return Some(cand.id);
            }
        }
    }
    None
}

/// Finds the recipient's newly scanned `NativeSong` matching `target_file` and
/// teaches `navidrome_libraries` the recipient's library root so delete_guard
/// always resolves it locally.
async fn learn_recipient_song(
    guard: &Guard<'_>,
    recipient_id: &str,
    source_song: &NativeSong,
    target_file: &Path,
) -> Option<String> {
    let recipient = UserId::new(recipient_id);
    let item = item_for_song(source_song, target_file);
    let candidates = match guard.listeners.songs_for_item(&recipient, &item).await {
        Ok(Some(c)) => c,
        _ => return None,
    };
    if let Err(e) = paths::learn_in(guard.db, &candidates, target_file).await {
        warn!("Could not learn recipient Navidrome library root: {e}");
    }
    let canon_target = std::fs::canonicalize(target_file).ok()?;
    for cand in candidates {
        if let Ok((local, _)) =
            paths::local_file_learning(guard.db, &cand, guard.music_path, &recipient).await
        {
            if local == canon_target {
                return Some(cand.id);
            }
        }
    }
    None
}

/// Helper to summarize share counts by link mode for logs/UI.
pub fn summarize_link_modes(entries: &[ShareResultEntry]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for e in entries {
        if let Some(mode) = e.link_mode {
            *counts.entry(mode.as_str().to_string()).or_insert(0) += 1;
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_ctx;
    use crate::db::test_db;

    #[tokio::test]
    async fn list_recipients_excludes_self_opted_out_and_folderless_users() {
        let db = test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;
        crate::core::folder_of(&db, "bob").await;
        crate::core::folder_of(&db, "carol").await;
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ('dave', 'dave', 'x')")
            .execute(&db)
            .await
            .unwrap();
        UserSettings::upsert_in(
            &db,
            "carol",
            crate::models::user_settings::UpdateUserSettings {
                accept_shares: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let recipients = list_recipients_in(&db, &test_ctx("alice")).await.unwrap();
        assert_eq!(
            recipients,
            vec![ShareRecipient {
                user_id: "bob".into(),
                username: "bob".into(),
            }]
        );
    }
}
