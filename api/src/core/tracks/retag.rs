//! Re-tagging and MusicBrainz re-matching of existing library tracks via beets without losing
//! Navidrome song IDs, ratings, stars, play counts, or playlist entries.
//!
//! By default (`relocate: false`), tags are written in place while keeping the file at its exact
//! path, so Navidrome updates the existing `media_file` row in place.
//! When `relocate: true`, Soulbeet runs a 2-scan dance:
//!   1. Write tags in place (path unchanged).
//!   2. Trigger Navidrome scan #1 and wait until it completes (Navidrome updates tags + PID on the
//!      existing `song_id`).
//!   3. Run `beet move` (`relocate_file`), move track sidecars, and prune empty folders.
//!   4. Trigger Navidrome scan #2 and verify `song_id` survived.

use std::path::PathBuf;

use serde::Serialize;
use shared::download::MatchChoices;
use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage, UserAction, UserActionCode};
use soulbeet::navidrome::models::NativeSong;
use soulbeet::{
    BeetsTrackInfo, DriverChoice, ImportMode, ImportOutcome, RegisterOutcome, SkipReason, TagEdits,
};
use tracing::info;

use super::{
    can_trigger_scan, find_sidecars, transfer_sidecars, trigger_and_wait_scan, wait_for_idle_scan,
    ScanWaitOutcome, TrackSelection, ALBUM_MAX_TRACKS, IDLE_SCAN_WAIT_MAX, SCAN_WAIT_MAX,
    SONGS_PER_CALL,
};
use crate::auth::UserCtx;
use crate::core::database_failure;
use crate::core::delete_guard::quota::deleting_app_in;
use crate::core::delete_guard::{paths, rules, Guard, Origin};
use crate::core::index::NavidromeOf;
use crate::core::library::{album_not_found, SongFacts};
use crate::folder_policy::PolicyRows;
use crate::import_queue::ImportQueue;
use crate::models::discovery_playlist::DiscoveryTrackRow;

#[derive(Debug, Clone)]
pub enum RetagMode {
    /// Direct tag field edits applied with `beet modify -y -M -w`.
    Direct(TagEdits),
    /// MusicBrainz re-match applied with `beet import -C -M` (and `-p -W` on dry_run).
    Rematch {
        choice: DriverChoice,
        album_mode: bool,
    },
}

#[derive(Debug, Clone)]
pub struct RetagRequest {
    pub selection: TrackSelection,
    pub mode: RetagMode,
    pub dry_run: bool,
    pub relocate: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RetagItemSuccess {
    pub song_id: String,
    /// `"would_retag"`, `"retagged"`, `"retagged_pending_scan"`, or `"retagged_relocated"`.
    pub status: String,
    pub song: SongFacts,
    pub file: String,
    pub new_file: Option<String>,
    pub song_id_preserved: Option<bool>,
    pub beets: Option<BeetsTrackInfo>,
    pub candidates: Option<MatchChoices>,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub struct RetagEntry {
    pub song_id: String,
    pub outcome: Result<RetagItemSuccess, Failure>,
}

#[derive(Debug)]
pub struct RetagReport {
    pub dry_run: bool,
    pub relocated: bool,
    pub scan_status: Option<String>,
    pub entries: Vec<RetagEntry>,
}

struct CheckedTrack {
    idx: usize,
    song: NativeSong,
    folder_root: PathBuf,
    file: PathBuf,
}

/// Checks and (when `!req.dry_run`) re-tags or re-matches tracks in place via beets.
pub(crate) async fn retag_tracks(
    guard: &Guard<'_>,
    navidrome: &dyn NavidromeOf,
    queue: &ImportQueue,
    ctx: &UserCtx,
    origin: &Origin,
    req: RetagRequest,
) -> Result<RetagReport, Failure> {
    let user = ctx.id();
    if let RetagMode::Direct(edits) = &req.mode {
        if edits.is_empty() {
            return Err(Failure::new(
                FailureCode::InvalidInput,
                Stage::Input,
                RetryAdvice::NotRetryable,
                "At least one tag field or rematch mode must be provided",
            ));
        }
    }

    let song_ids = match &req.selection {
        TrackSelection::Songs(ids) => {
            if ids.is_empty() || ids.len() > SONGS_PER_CALL {
                return Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Input,
                    RetryAdvice::NotRetryable,
                    format!("Provide between 1 and {SONGS_PER_CALL} distinct song ids"),
                ));
            }
            ids.clone()
        }
        TrackSelection::Album(aid) => {
            let album = guard
                .listeners
                .album(&user, aid)
                .await
                .map_err(|e| e.failure(Stage::Retag, e.to_string()))?
                .ok_or_else(|| album_not_found(aid))?;
            if album.song.is_empty() {
                return Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Retag,
                    RetryAdvice::NotRetryable,
                    "This album has no songs to re-tag",
                ));
            }
            if album.song.len() > ALBUM_MAX_TRACKS {
                return Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Input,
                    RetryAdvice::NotRetryable,
                    format!(
                        "This album has {} tracks, over the {ALBUM_MAX_TRACKS}-track limit",
                        album.song.len()
                    ),
                ));
            }
            album.song.into_iter().map(|s| s.id).collect()
        }
    };

    if !req.dry_run {
        if let Origin::Mcp { grant_id } = origin {
            let mut conn = guard
                .db
                .acquire()
                .await
                .map_err(|e| database_failure(Stage::Retag, e))?;
            let _ = deleting_app_in(&mut conn, &user, grant_id)
                .await
                .map_err(|f| Failure {
                    stage: Stage::Retag,
                    ..f
                })?;
        }
    }

    let own_navi = navidrome
        .navidrome_of(&user)
        .await
        .map_err(|e| e.failure(Stage::Retag, e.to_string()))?;

    // Relocating requires an intermediate Navidrome scan so changing both tags and filename
    // never breaks Navidrome's song_id matching.
    if req.relocate && !req.dry_run && !can_trigger_scan(guard.db, navidrome, &user, &own_navi).await? {
        return Err(Failure::new(
            FailureCode::RetagRelocateNeedsScan,
            Stage::Retag,
            RetryAdvice::NeedsUser,
            "Relocating after re-tagging requires a linked Navidrome admin account to run an intermediate scan so song IDs and ratings are preserved; omit relocate (or set relocate: false) to re-tag in place",
        )
        .with_user_action(UserAction::new(
            UserActionCode::EnableIndexing,
            "Sign in to Soulbeet once as a Navidrome admin, or re-tag without relocate",
            "Soulbeet sign-in",
        )));
    }

    let rows = PolicyRows::load_in(guard.db)
        .await
        .map_err(|e| database_failure(Stage::Retag, e))?;
    let (policy_folders, _) = rows.inputs();
    let pending_discovery = DiscoveryTrackRow::pending_for_user_in(guard.db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Retag, e))?;

    let mut entries: Vec<Option<RetagEntry>> = (0..song_ids.len()).map(|_| None).collect();
    let mut checked: Vec<CheckedTrack> = Vec::new();

    for (idx, song_id) in song_ids.into_iter().enumerate() {
        let song = match guard.read_song(&user, &song_id).await {
            Ok(Some(s)) => s,
            Ok(None) => {
                entries[idx] = Some(RetagEntry {
                    song_id,
                    outcome: Err(Failure::new(
                        FailureCode::UnknownHandle,
                        Stage::Retag,
                        RetryAdvice::NotRetryable,
                        "Navidrome has no song with this id that this account can see",
                    )),
                });
                continue;
            }
            Err(f) => {
                entries[idx] = Some(RetagEntry {
                    song_id,
                    outcome: Err(Failure { stage: Stage::Retag, ..f }),
                });
                continue;
            }
        };

        if pending_discovery
            .iter()
            .any(|t| t.song_id.as_deref() == Some(song.id.as_str()))
        {
            entries[idx] = Some(RetagEntry {
                song_id,
                outcome: Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Retag,
                    RetryAdvice::NotRetryable,
                    "Cannot re-tag a pending discovery track; promote it into your library first",
                )),
            });
            continue;
        }

        let (file, _) = match paths::local_file_learning(guard.db, &song, guard.music_path, &user).await
        {
            Ok(found) => found,
            Err(f) => {
                entries[idx] = Some(RetagEntry {
                    song_id,
                    outcome: Err(Failure { stage: Stage::Retag, ..f }),
                });
                continue;
            }
        };

        let folder_root = match rules::owning_folders(&file, &policy_folders, &user) {
            Ok(root) => root.to_path_buf(),
            Err(f) => {
                entries[idx] = Some(RetagEntry {
                    song_id,
                    outcome: Err(Failure { stage: Stage::Retag, ..f }),
                });
                continue;
            }
        };

        if let Err(f) = rules::audio_file(&file) {
            entries[idx] = Some(RetagEntry {
                song_id,
                outcome: Err(Failure { stage: Stage::Retag, ..f }),
            });
            continue;
        }

        let holding = rules::holding_folders(&file, &policy_folders);
        if let Err(f) = rules::not_in_use_in(guard.db, &holding, &song, &[]).await {
            entries[idx] = Some(RetagEntry {
                song_id,
                outcome: Err(Failure { stage: Stage::Retag, ..f }),
            });
            continue;
        }

        checked.push(CheckedTrack {
            idx,
            song,
            folder_root,
            file,
        });
    }

    // Dry run: preview direct tag edits or run pretend rematch.
    if req.dry_run {
        for track in checked {
            match &req.mode {
                RetagMode::Direct(_) => {
                    let current = queue
                        .inspect_file(&track.folder_root, &track.file)
                        .await
                        .ok()
                        .flatten();
                    entries[track.idx] = Some(RetagEntry {
                        song_id: track.song.id.clone(),
                        outcome: Ok(RetagItemSuccess {
                            song_id: track.song.id.clone(),
                            status: "would_retag".into(),
                            song: SongFacts::from(&track.song),
                            file: track.file.to_string_lossy().into_owned(),
                            new_file: None,
                            song_id_preserved: None,
                            beets: current,
                            candidates: None,
                            warnings: vec![],
                        }),
                    });
                }
                RetagMode::Rematch { choice, .. } => {
                    let res = queue
                        .rematch_in_place(
                            &track.folder_root,
                            vec![track.file.clone()],
                            ImportMode::Singleton,
                            choice.clone(),
                            true,
                        )
                        .await;
                    entries[track.idx] = Some(RetagEntry {
                        song_id: track.song.id.clone(),
                        outcome: Ok(RetagItemSuccess {
                            song_id: track.song.id.clone(),
                            status: "would_retag".into(),
                            song: SongFacts::from(&track.song),
                            file: track.file.to_string_lossy().into_owned(),
                            new_file: None,
                            song_id_preserved: None,
                            beets: None,
                            candidates: res.choices,
                            warnings: vec![],
                        }),
                    });
                }
            }
        }
        return Ok(RetagReport {
            dry_run: true,
            relocated: false,
            scan_status: None,
            entries: entries.into_iter().flatten().collect(),
        });
    }

    if checked.is_empty() {
        return Ok(RetagReport {
            dry_run: false,
            relocated: false,
            scan_status: None,
            entries: entries.into_iter().flatten().collect(),
        });
    }

    let _ = wait_for_idle_scan(&own_navi, IDLE_SCAN_WAIT_MAX).await;

    struct Retagged {
        track: CheckedTrack,
        beets: Option<BeetsTrackInfo>,
        candidates: Option<MatchChoices>,
        warnings: Vec<String>,
    }

    let mut updated: Vec<Retagged> = Vec::new();
    for track in checked {
        // Copy-on-Write: break any hardlink before beets modifies tags in place so
        // another user's shared copy is never mutated.
        let cow_path = track.file.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || super::cow::break_hardlink_if_shared(&cow_path))
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))
            .and_then(|r| r)
        {
            entries[track.idx] = Some(RetagEntry {
                song_id: track.song.id,
                outcome: Err(Failure::new(
                    FailureCode::RetagFailed,
                    Stage::Retag,
                    RetryAdvice::SameSourceLater,
                    format!("Could not isolate shared file before re-tagging: {e}"),
                )
                .with_cause(Service::Filesystem, None, e.to_string())),
            });
            continue;
        }
        match &req.mode {
            RetagMode::Direct(edits) => {
                match queue
                    .modify_tags(&track.folder_root, &track.file, edits.clone())
                    .await
                {
                    Ok(info) => {
                        info!("Modified tags in place for {}", track.file.display());
                        updated.push(Retagged {
                            track,
                            beets: Some(info),
                            candidates: None,
                            warnings: vec![],
                        });
                    }
                    Err(f) => {
                        entries[track.idx] = Some(RetagEntry {
                            song_id: track.song.id,
                            outcome: Err(f),
                        });
                    }
                }
            }
            RetagMode::Rematch { choice, album_mode } => {
                // Forget existing beets item first so beets doesn't see the file as a duplicate
                // of itself; re-register it if rematch skips or fails.
                let _ = queue.forget(&track.folder_root, &track.file).await;
                let mode = if *album_mode {
                    ImportMode::Album
                } else {
                    ImportMode::Singleton
                };
                let res = queue
                    .rematch_in_place(
                        &track.folder_root,
                        vec![track.file.clone()],
                        mode,
                        choice.clone(),
                        false,
                    )
                    .await;
                match res.outcome {
                    ImportOutcome::Imported { warnings, .. } => {
                        let info = queue
                            .inspect_file(&track.folder_root, &track.file)
                            .await
                            .ok()
                            .flatten();
                        info!("Re-matched tags in place for {}", track.file.display());
                        updated.push(Retagged {
                            track,
                            beets: info,
                            candidates: res.choices,
                            warnings,
                        });
                    }
                    ImportOutcome::Skipped(SkipReason::NoMatch { candidates }) => {
                        let _ = queue
                            .register_in_place(&track.folder_root, &track.file)
                            .await;
                        let msg = match candidates {
                            Some(n) => format!("No close MusicBrainz match ({n} candidates); file tags unchanged"),
                            None => "No close MusicBrainz match; file tags unchanged".into(),
                        };
                        entries[track.idx] = Some(RetagEntry {
                            song_id: track.song.id,
                            outcome: Err(Failure::new(
                                FailureCode::ImportNoMatch,
                                Stage::Retag,
                                RetryAdvice::NeedsDecision,
                                msg,
                            )),
                        });
                    }
                    other => {
                        if let RegisterOutcome::Failed(e) = queue
                            .register_in_place(&track.folder_root, &track.file)
                            .await
                        {
                            tracing::warn!(
                                "Could not restore beets entry for {} after failed rematch: {e}",
                                track.file.display()
                            );
                        }
                        entries[track.idx] = Some(RetagEntry {
                            song_id: track.song.id,
                            outcome: Err(Failure::new(
                                FailureCode::RetagFailed,
                                Stage::Retag,
                                RetryAdvice::SameSourceLater,
                                format!("Could not re-match track: {other}"),
                            )),
                        });
                    }
                }
            }
        }
    }

    if updated.is_empty() {
        return Ok(RetagReport {
            dry_run: false,
            relocated: false,
            scan_status: None,
            entries: entries.into_iter().flatten().collect(),
        });
    }

    // Scan #1: Navidrome updates tags & PID in place on the existing song_id.
    let mut scan_outcome = trigger_and_wait_scan(
        guard.db,
        navidrome,
        &user,
        &own_navi,
        Stage::Retag,
        SCAN_WAIT_MAX,
    )
    .await;

    // Step 2 of the 2-scan dance when `relocate: true`.
    let mut any_relocated = false;
    let mut new_paths: Vec<Option<PathBuf>> = vec![None; updated.len()];
    if req.relocate {
        if scan_outcome == ScanWaitOutcome::Completed {
            for (i, item) in updated.iter_mut().enumerate() {
                let sidecars = find_sidecars(&item.track.file);
                let reloc = queue
                    .relocate_file(&item.track.folder_root, &item.track.file)
                    .await;
                if let Some(f) = reloc.failure {
                    item.warnings
                        .push(format!("Tags updated, but relocate failed: {}", f.message));
                    continue;
                }
                if let Some((_, rel_new)) = reloc.now.first() {
                    let abs_new = if rel_new.is_absolute() {
                        rel_new.clone()
                    } else {
                        item.track.folder_root.join(rel_new)
                    };
                    if abs_new != item.track.file {
                        any_relocated = true;
                        let new_stem = abs_new.file_stem().and_then(|s| s.to_str());
                        if let (Some(src_dir), Some(dst_dir)) =
                            (item.track.file.parent(), abs_new.parent())
                        {
                            item.warnings.extend(transfer_sidecars(
                                &sidecars,
                                src_dir,
                                dst_dir,
                                new_stem,
                            ));
                            let _ = crate::server_fns::cleanup_empty_ancestors(
                                src_dir,
                                &item.track.folder_root,
                            )
                            .await;
                        }
                        new_paths[i] = Some(abs_new);
                    }
                }
            }
            if any_relocated {
                // Scan #2: Navidrome matches the moved file to the updated PID and preserves song_id!
                scan_outcome = trigger_and_wait_scan(
                    guard.db,
                    navidrome,
                    &user,
                    &own_navi,
                    Stage::Retag,
                    SCAN_WAIT_MAX,
                )
                .await;
            }
        } else {
            for item in &mut updated {
                item.warnings.push(
                    "Skipped relocate because the intermediate Navidrome scan did not finish; tags were written in place so song_id stays safe".into(),
                );
            }
        }
    }

    let scan_status = Some(match &scan_outcome {
        ScanWaitOutcome::Completed => "completed".to_string(),
        ScanWaitOutcome::NoScanAccount => "no_scan_account".to_string(),
        ScanWaitOutcome::TimedOut => "timed_out".to_string(),
        ScanWaitOutcome::Failed(_) => "failed".to_string(),
    });

    for (mut item, moved_to) in updated.into_iter().zip(new_paths) {
        let (status, song_id_preserved, latest_song) = match &scan_outcome {
            ScanWaitOutcome::Completed => {
                match own_navi.get_native_song(&item.track.song.id).await {
                    Ok(Some(found)) if !found.missing => {
                        let status = if moved_to.is_some() {
                            "retagged_relocated"
                        } else {
                            "retagged"
                        };
                        (status.to_string(), Some(true), found)
                    }
                    _ => {
                        item.warnings.push(
                            "Navidrome scanned after re-tagging and did not return the original song_id".into(),
                        );
                        (
                            "retagged_pending_scan".to_string(),
                            Some(false),
                            item.track.song.clone(),
                        )
                    }
                }
            }
            _ => (
                "retagged_pending_scan".to_string(),
                None,
                item.track.song.clone(),
            ),
        };

        entries[item.track.idx] = Some(RetagEntry {
            song_id: item.track.song.id.clone(),
            outcome: Ok(RetagItemSuccess {
                song_id: item.track.song.id,
                status,
                song: SongFacts::from(&latest_song),
                file: item.track.file.to_string_lossy().into_owned(),
                new_file: moved_to.map(|p| p.to_string_lossy().into_owned()),
                song_id_preserved,
                beets: item.beets,
                candidates: item.candidates,
                warnings: item.warnings,
            }),
        });
    }

    Ok(RetagReport {
        dry_run: false,
        relocated: any_relocated,
        scan_status,
        entries: entries.into_iter().flatten().collect(),
    })
}
