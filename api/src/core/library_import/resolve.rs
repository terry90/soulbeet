//! Match Picker resolution for `needs_decision` and `failed` library import items.

use crate::auth::{UserCtx, UserId};
use crate::core::database_failure;
use crate::core::index::{NavidromeOf, ServerNavidrome};
use crate::core::library_import::runner::{
    classify_import_result, emit_batch_event, get_batch_detail_in, is_source_in_place,
    trigger_navidrome_scan_best_effort, LibraryImportCoordinator, COORDINATOR,
};
use crate::core::library_import::scan::validate_unit_before_beets;
use crate::core::tracks::cow::break_hardlink_if_shared;
use crate::db::DB;
use crate::import_queue::{
    musicbrainz_id, validate_tag_overrides, Duplicates, ImportJob, ImportQueue, IMPORT_QUEUE,
};
use crate::models::library_import::{
    claim_item_for_resolve_in, dismiss_item_cas_in, get_item_for_user_in, update_item_result_in,
};
use shared::download::{AsIsTagOverrides, ImportChoice};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::library_import::{
    LibraryImportBatchDetail, LibraryImportItem, LibraryImportItemStatus, LibraryImportMode,
    LibraryImportResolveAction,
};
use soulbeet::audio::collect_audio_files;
use soulbeet::tags::{write_tags, WantedTags};
use soulbeet::{DriverChoice, DuplicateRun, ImportMode, ImportPlacement};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// RAII temporary staging directory used when applying `AsIsWithTags` to an external
/// (`!in_place`) source file so the original external file is never mutated.
struct TempStagingDir(PathBuf);

impl TempStagingDir {
    fn create() -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "soulbeet-import-stage-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempStagingDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn item_not_found() -> Failure {
    Failure::new(
        FailureCode::UnknownHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Library import item not found",
    )
}

fn wrong_item_state(status: LibraryImportItemStatus) -> Failure {
    Failure::new(
        FailureCode::WrongJobState,
        Stage::Import,
        RetryAdvice::NotRetryable,
        format!(
            "Library import item is {}, only needs_decision or failed items can be resolved",
            status.as_str()
        ),
    )
}

fn item_already_claimed() -> Failure {
    Failure::new(
        FailureCode::WrongJobState,
        Stage::Import,
        RetryAdvice::NotRetryable,
        "Library import item is already being resolved or is no longer in needs_decision or failed state",
    )
}

/// Resolves a single `needs_decision` or `failed` library import item (live DB & queue).
pub async fn resolve_item(
    actor: &UserCtx,
    item_id: &str,
    action: LibraryImportResolveAction,
) -> Result<LibraryImportBatchDetail, Failure> {
    resolve_item_in(
        COORDINATOR.clone(),
        &DB,
        &IMPORT_QUEUE,
        Arc::new(ServerNavidrome),
        actor,
        item_id,
        action,
    )
    .await
}

pub(crate) async fn resolve_item_in(
    coordinator: Arc<LibraryImportCoordinator>,
    db: &SqlitePool,
    queue: &ImportQueue,
    navidrome: Arc<dyn NavidromeOf>,
    actor: &UserCtx,
    item_id: &str,
    action: LibraryImportResolveAction,
) -> Result<LibraryImportBatchDetail, Failure> {
    let (batch, item) =
        get_item_for_user_in(db, item_id, &actor.user_id, actor.role.is_admin())
            .await
            .map_err(|e| database_failure(Stage::Import, e))?
            .ok_or_else(item_not_found)?;

    if !matches!(
        item.status,
        LibraryImportItemStatus::NeedsDecision | LibraryImportItemStatus::Failed
    ) {
        return Err(wrong_item_state(item.status));
    }

    match action {
        LibraryImportResolveAction::Dismiss => {
            // CRITICAL SAFETY INVARIANT: Dismiss NEVER deletes any file on disk.
            let Some((updated_batch, updated_item)) =
                dismiss_item_cas_in(db, &batch.id, &item.id)
                    .await
                    .map_err(|e| database_failure(Stage::Import, e))?
            else {
                return Err(item_already_claimed());
            };
            emit_batch_event(&updated_batch, Some(updated_item)).await;
            get_batch_detail_in(db, actor, Some(&batch.id), None, 50, 0).await
        }
        LibraryImportResolveAction::Choose(choice) => {
            let import_mode = if item.is_album_dir {
                ImportMode::Album
            } else {
                ImportMode::Singleton
            };
            let (driver_choice, tag_overrides) =
                resolve_choice(&choice, batch.mode, &item, import_mode)?;

            // Validate before claiming so symlink/escape/missing-file errors fail cleanly
            // without transitioning the item into importing.
            validate_unit_before_beets(&batch, &item).await?;

            // Atomically claim the item (CAS) before writing tags or submitting to beets.
            let claimed = claim_item_for_resolve_in(db, &item.id)
                .await
                .map_err(|e| database_failure(Stage::Import, e))?;
            if !claimed {
                return Err(item_already_claimed());
            }

            let resolving_guard = coordinator.track_resolving(&item.id);
            let db_bg = db.clone();
            let queue_bg = queue.clone();
            let batch_bg = batch.clone();
            let item_bg = item.clone();

            // Run tagging, beets execution, DB result persistence, counter recalculation,
            // WS notification, and Navidrome scan inside a detached spawned task so a client
            // disconnect or proxy timeout can NEVER abandon the item in `importing`.
            let handle = tokio::spawn(async move {
                let _resolving_guard = resolving_guard;

                let canon_item_path = match validate_unit_before_beets(&batch_bg, &item_bg).await {
                    Ok(p) => p,
                    Err(failure) => {
                        if let Ok((b, updated)) = update_item_result_in(
                            &db_bg,
                            &batch_bg.id,
                            &item_bg.id,
                            LibraryImportItemStatus::Failed,
                            None,
                            Some(&failure),
                            None,
                        )
                        .await
                        {
                            emit_batch_event(&b, Some(updated)).await;
                        }
                        return Err(failure);
                    }
                };

                let in_place = is_source_in_place(&batch_bg.source_path, &batch_bg.target_path);
                let mut _staging_guard: Option<TempStagingDir> = None;
                let effective_item_path = if let Some(overrides) = tag_overrides {
                    if in_place {
                        if let Err(failure) =
                            stamp_singleton_tags_with_cow(&canon_item_path, &overrides).await
                        {
                            if let Ok((b, updated)) = update_item_result_in(
                                &db_bg,
                                &batch_bg.id,
                                &item_bg.id,
                                item_bg.status,
                                item_bg.skip_kind.as_deref(),
                                Some(&failure),
                                item_bg.choices.as_ref(),
                            )
                            .await
                            {
                                emit_batch_event(&b, Some(updated)).await;
                            }
                            return Err(failure);
                        }
                        canon_item_path
                    } else {
                        // External source folder: NEVER mutate the original file. Stage a
                        // copy in a temporary directory, write tags on the copy, and let
                        // beets CopyOnly from the staged copy before dropping the temp dir.
                        match stage_and_tag_external_singleton(&canon_item_path, &overrides).await {
                            Ok((staging, staged_file)) => {
                                _staging_guard = Some(staging);
                                staged_file
                            }
                            Err(failure) => {
                                if let Ok((b, updated)) = update_item_result_in(
                                    &db_bg,
                                    &batch_bg.id,
                                    &item_bg.id,
                                    item_bg.status,
                                    item_bg.skip_kind.as_deref(),
                                    Some(&failure),
                                    item_bg.choices.as_ref(),
                                )
                                .await
                                {
                                    emit_batch_event(&b, Some(updated)).await;
                                }
                                return Err(failure);
                            }
                        }
                    }
                } else {
                    canon_item_path
                };

                let target_path = PathBuf::from(&batch_bg.target_path);
                let result = match batch_bg.mode {
                    LibraryImportMode::TagAndOrganize => {
                        let files = collect_audio_files(&[effective_item_path.as_path()]).await;
                        let placement = if in_place {
                            ImportPlacement::MoveInPlace
                        } else {
                            ImportPlacement::CopyOnly
                        };
                        queue_bg
                            .submit(ImportJob {
                                sources: vec![effective_item_path],
                                files,
                                target: target_path,
                                mode: import_mode,
                                owner: None,
                                snapshot_target: false,
                                cleanup_root: None,
                                choice: driver_choice,
                                choices: item_bg.choices.clone().map(Arc::new),
                                duplicates: Duplicates::Skip,
                                last_run: None,
                                lock_retries: 0,
                                placement,
                            })
                            .await
                            .wait()
                            .await
                            .result
                    }
                    LibraryImportMode::TagInPlace => {
                        queue_bg
                            .import_in_place(
                                &target_path,
                                vec![effective_item_path],
                                import_mode,
                                driver_choice,
                                DuplicateRun::Skip,
                            )
                            .await
                    }
                    LibraryImportMode::RegisterAsIs => {
                        queue_bg
                            .register_in_place_batch(
                                &target_path,
                                vec![effective_item_path],
                                import_mode,
                            )
                            .await
                    }
                };

                // Drop temporary staging directory now that beets has finished copying.
                drop(_staging_guard);

                let (status, skip_kind, failure, choices) =
                    classify_import_result(&result, item_bg.is_album_dir, item_bg.choices.as_ref());

                let (updated_batch, updated_item) = update_item_result_in(
                    &db_bg,
                    &batch_bg.id,
                    &item_bg.id,
                    status,
                    skip_kind.as_deref(),
                    failure.as_ref(),
                    choices.as_ref(),
                )
                .await
                .map_err(|e| database_failure(Stage::Import, e))?;

                emit_batch_event(&updated_batch, Some(updated_item)).await;
                if status == LibraryImportItemStatus::Imported {
                    trigger_navidrome_scan_best_effort(
                        db_bg.clone(),
                        navidrome,
                        UserId::new(&updated_batch.user_id),
                    );
                }
                Ok(())
            });

            match handle.await {
                Ok(Ok(())) => get_batch_detail_in(db, actor, Some(&batch.id), None, 50, 0).await,
                Ok(Err(failure)) => Err(failure),
                Err(join_err) => {
                    let failure = Failure::new(
                        FailureCode::ImportBeetsFailed,
                        Stage::Import,
                        RetryAdvice::SameSourceLater,
                        format!("Library import resolution task failed unexpectedly: {join_err}"),
                    );
                    if let Ok((b, updated)) = update_item_result_in(
                        db,
                        &batch.id,
                        &item.id,
                        LibraryImportItemStatus::Failed,
                        None,
                        Some(&failure),
                        item.choices.as_ref(),
                    )
                    .await
                    {
                        emit_batch_event(&b, Some(updated)).await;
                    }
                    Err(failure)
                }
            }
        }
    }
}

fn resolve_choice(
    choice: &ImportChoice,
    batch_mode: LibraryImportMode,
    item: &LibraryImportItem,
    mode: ImportMode,
) -> Result<(DriverChoice, Option<AsIsTagOverrides>), Failure> {
    if batch_mode == LibraryImportMode::RegisterAsIs
        && matches!(
            choice,
            ImportChoice::Candidate { .. } | ImportChoice::MusicBrainz(_)
        )
    {
        return Err(Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NeedsDecision,
            "Register-as-is batches do not run metadata matching; use as_is, as_is_with_tags (for singletons), or dismiss",
        ));
    }
    match choice {
        ImportChoice::Auto => Ok((DriverChoice::Auto, None)),
        ImportChoice::AsIs => Ok((DriverChoice::AsIs, None)),
        ImportChoice::AsIsWithTags(overrides) => {
            if item.is_album_dir {
                return Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Input,
                    RetryAdvice::NeedsDecision,
                    "Tagging and importing as-is is only supported for single-track imports",
                ));
            }
            let valid = validate_tag_overrides(overrides).map_err(|r| r.failure)?;
            Ok((DriverChoice::AsIs, Some(valid)))
        }
        ImportChoice::Candidate { data_source, id } => {
            let Some(choices) = &item.choices else {
                return Err(Failure::new(
                    FailureCode::ImportChoiceInvalid,
                    Stage::Input,
                    RetryAdvice::NeedsDecision,
                    "This import item has no matches to pick from",
                ));
            };
            let found = choices
                .candidates
                .iter()
                .any(|c| c.data_source == *data_source && c.id == *id);
            if !found {
                return Err(Failure::new(
                    FailureCode::ImportChoiceInvalid,
                    Stage::Input,
                    RetryAdvice::NeedsDecision,
                    "That match is not one beets found for this import item",
                ));
            }
            Ok((
                DriverChoice::Pick {
                    data_source: data_source.clone(),
                    id: id.clone(),
                },
                None,
            ))
        }
        ImportChoice::MusicBrainz(text) => {
            let id = musicbrainz_id(text, mode).map_err(|r| r.failure)?;
            Ok((
                DriverChoice::Pick {
                    data_source: "MusicBrainz".to_string(),
                    id,
                },
                None,
            ))
        }
    }
}

fn wanted_tags_from_overrides(overrides: &AsIsTagOverrides) -> WantedTags {
    WantedTags {
        title: overrides.title.clone(),
        artist: overrides.artist.clone(),
        album: overrides.album.clone().unwrap_or_default(),
        album_artist: overrides.artist.clone(),
        track_no: None,
        track_total: None,
        date: None,
        musicbrainz: None,
        comment: None,
    }
}

/// Breaks any shared hardlink via CoW and writes minimal tags onto a validated in-place
/// singleton file.
async fn stamp_singleton_tags_with_cow(
    canon_file: &Path,
    overrides: &AsIsTagOverrides,
) -> Result<(), Failure> {
    let target = canon_file.to_path_buf();
    let wanted = wanted_tags_from_overrides(overrides);

    tokio::task::spawn_blocking(move || -> Result<(), String> {
        break_hardlink_if_shared(&target)
            .map_err(|e| format!("Could not isolate shared hardlink before tagging: {e}"))?;
        write_tags(&target, &wanted).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| {
        Failure::new(
            FailureCode::ImportBeetsFailed,
            Stage::Import,
            RetryAdvice::SameSourceLater,
            format!("Tag writer task panicked: {e}"),
        )
    })?
    .map_err(|e| {
        Failure::new(
            FailureCode::ImportChoiceInvalid,
            Stage::Import,
            RetryAdvice::NeedsDecision,
            format!("Could not write tags before as-is import: {e}"),
        )
    })
}

/// Copies a validated external singleton file into a fresh temporary staging directory and
/// writes the requested tags onto the staged copy so the external source file is never mutated.
async fn stage_and_tag_external_singleton(
    canon_file: &Path,
    overrides: &AsIsTagOverrides,
) -> Result<(TempStagingDir, PathBuf), Failure> {
    let source = canon_file.to_path_buf();
    let wanted = wanted_tags_from_overrides(overrides);

    tokio::task::spawn_blocking(move || -> Result<(TempStagingDir, PathBuf), String> {
        let staging = TempStagingDir::create()
            .map_err(|e| format!("Could not create temporary staging directory: {e}"))?;
        let file_name = source
            .file_name()
            .ok_or_else(|| "Source file has no filename".to_string())?;
        let staged_path = staging.path().join(file_name);
        std::fs::copy(&source, &staged_path)
            .map_err(|e| format!("Could not copy external source file to staging: {e}"))?;
        write_tags(&staged_path, &wanted).map_err(|e| e.to_string())?;
        Ok((staging, staged_path))
    })
    .await
    .map_err(|e| {
        Failure::new(
            FailureCode::ImportBeetsFailed,
            Stage::Import,
            RetryAdvice::SameSourceLater,
            format!("External staging tag task panicked: {e}"),
        )
    })?
    .map_err(|e| {
        Failure::new(
            FailureCode::ImportChoiceInvalid,
            Stage::Import,
            RetryAdvice::NeedsDecision,
            format!("Could not write tags to staged copy before as-is import: {e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Role;
    use crate::db::test_db;
    use crate::import_queue::test_support::{
        candidate, imported, track_choices, RecordingDeps, StubImporter,
    };
    use crate::models::library_import::{
        create_batch_with_items_in, update_item_result_in, NewLibraryImportBatch,
        NewLibraryImportItem,
    };
    use crate::test_navidrome::NavidromeFor;
    use shared::library_import::{LibraryImportBatchStatus, LibraryImportOrigin};

    fn alice_ctx() -> UserCtx {
        UserCtx {
            user_id: "alice".into(),
            username: "alice".into(),
            role: Role::User,
        }
    }

    fn test_navidrome() -> Arc<dyn NavidromeOf> {
        Arc::new(NavidromeFor(Err(|| {
            crate::services::NavidromeLookupError::NotLinked
        })))
    }

    fn minimal_flac() -> Vec<u8> {
        let mut out = b"fLaC".to_vec();
        out.extend([0x80, 0, 0, 34]);
        out.extend(4096u16.to_be_bytes());
        out.extend(4096u16.to_be_bytes());
        out.extend([0; 6]);
        let packed: u64 = (44_100u64 << 44) | (15 << 36) | 4096;
        out.extend(packed.to_be_bytes());
        out.extend([0; 16]);
        let mut frame = vec![0xFF, 0xF8, 0xC9, 0x08, 0];
        let mut crc8 = 0u8;
        for &b in &frame {
            crc8 ^= b;
            for _ in 0..8 {
                crc8 = if crc8 & 0x80 != 0 {
                    (crc8 << 1) ^ 0x07
                } else {
                    crc8 << 1
                };
            }
        }
        frame.push(crc8);
        frame.extend([0x00, 0x00, 0x00]);
        let mut crc16 = 0u16;
        for &b in &frame {
            crc16 ^= u16::from(b) << 8;
            for _ in 0..8 {
                crc16 = if crc16 & 0x8000 != 0 {
                    (crc16 << 1) ^ 0x8005
                } else {
                    crc16 << 1
                };
            }
        }
        frame.extend(crc16.to_be_bytes());
        out.extend(frame);
        out
    }

    async fn seed_needs_decision_item(
        db: &SqlitePool,
        lib: &Path,
        item_path: &Path,
    ) -> (String, String) {
        let user_exists: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE id = 'alice'")
            .fetch_one(db)
            .await
            .unwrap();
        if user_exists.0 == 0 {
            sqlx::query("INSERT INTO users (id, username, password_hash, role) VALUES ('alice', 'alice', 'x', 'user')")
                .execute(db)
                .await
                .unwrap();
        }
        let folder_exists: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM folders WHERE id = 'f-alice'")
                .fetch_one(db)
                .await
                .unwrap();
        if folder_exists.0 == 0 {
            sqlx::query("INSERT INTO folders (id, user_id, name, path) VALUES ('f-alice', 'alice', 'Music', ?)")
                .bind(lib.to_str().unwrap())
                .execute(db)
                .await
                .unwrap();
        }

        let batch_id = uuid::Uuid::new_v4().to_string();
        let item_id = uuid::Uuid::new_v4().to_string();
        create_batch_with_items_in(
            db,
            &NewLibraryImportBatch {
                id: batch_id.clone(),
                user_id: "alice".into(),
                target_folder_id: "f-alice".into(),
                target_path: lib.to_string_lossy().into_owned(),
                source_path: lib.to_string_lossy().into_owned(),
                mode: LibraryImportMode::TagInPlace,
                status: LibraryImportBatchStatus::Completed,
                total_units: 1,
                total_files: 1,
                already_indexed_files: 0,
                origin: LibraryImportOrigin::Ui,
            },
            &[NewLibraryImportItem {
                id: item_id.clone(),
                seq: 0,
                source_path: item_path.to_string_lossy().into_owned(),
                display_name: "track.flac".into(),
                is_album_dir: false,
                file_count: 1,
            }],
        )
        .await
        .unwrap();

        let choices = track_choices(vec![candidate("MusicBrainz", "rec-1")]);
        update_item_result_in(
            db,
            &batch_id,
            &item_id,
            LibraryImportItemStatus::NeedsDecision,
            Some("no_match"),
            None,
            Some(&choices),
        )
        .await
        .unwrap();

        (batch_id, item_id)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_as_is_with_tags_breaks_hardlink_and_imports() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let alice_lib = root.join("music/alice");
        let bob_lib = root.join("music/bob");
        std::fs::create_dir_all(&alice_lib).unwrap();
        std::fs::create_dir_all(&bob_lib).unwrap();

        let bytes = minimal_flac();
        let alice_file = alice_lib.join("shared.flac");
        let bob_file = bob_lib.join("shared.flac");
        std::fs::write(&alice_file, &bytes).unwrap();
        std::fs::hard_link(&alice_file, &bob_file).unwrap();
        assert_eq!(std::fs::symlink_metadata(&alice_file).unwrap().nlink(), 2);

        let db = test_db::migrated().await;
        let (_batch_id, item_id) = seed_needs_decision_item(&db, &alice_lib, &alice_file).await;

        let stub = Arc::new(StubImporter::new());
        stub.script(imported());
        let queue = ImportQueue::new(RecordingDeps::new(stub.clone()));

        let detail = resolve_item_in(
            Arc::new(LibraryImportCoordinator::default()),
            &db,
            &queue,
            test_navidrome(),
            &alice_ctx(),
            &item_id,
            LibraryImportResolveAction::Choose(ImportChoice::AsIsWithTags(AsIsTagOverrides {
                artist: "Burial".into(),
                title: "Archangel".into(),
                album: Some("Untrue".into()),
            })),
        )
        .await
        .unwrap();

        assert_eq!(detail.batch.imported_units, 1);
        assert_eq!(detail.batch.needs_decision_units, 0);
        assert_eq!(detail.items[0].status, LibraryImportItemStatus::Imported);

        // CoW broke the hardlink: Bob's copy has untouched original bytes and a distinct inode.
        assert_eq!(std::fs::symlink_metadata(&alice_file).unwrap().nlink(), 1);
        assert_eq!(std::fs::symlink_metadata(&bob_file).unwrap().nlink(), 1);
        assert_ne!(
            std::fs::symlink_metadata(&alice_file).unwrap().ino(),
            std::fs::symlink_metadata(&bob_file).unwrap().ino()
        );
        assert_eq!(std::fs::read(&bob_file).unwrap(), bytes);
        assert_ne!(std::fs::read(&alice_file).unwrap(), bytes);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_as_is_with_tags_refuses_symlinks_and_outside_paths_and_dismiss_keeps_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let alice_lib = root.join("music/alice");
        let outside = root.join("outside");
        std::fs::create_dir_all(&alice_lib).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let bytes = minimal_flac();
        let secret = outside.join("secret.flac");
        std::fs::write(&secret, &bytes).unwrap();
        let symlink_path = alice_lib.join("escape.flac");
        std::os::unix::fs::symlink(&secret, &symlink_path).unwrap();

        let db = test_db::migrated().await;
        let (_b1, item_symlink) = seed_needs_decision_item(&db, &alice_lib, &symlink_path).await;
        let (_b2, item_outside) = seed_needs_decision_item(&db, &alice_lib, &secret).await;

        let stub = Arc::new(StubImporter::new());
        let queue = ImportQueue::new(RecordingDeps::new(stub));
        let coord = Arc::new(LibraryImportCoordinator::default());

        let action = LibraryImportResolveAction::Choose(ImportChoice::AsIsWithTags(
            AsIsTagOverrides {
                artist: "Artist".into(),
                title: "Title".into(),
                album: None,
            },
        ));

        let err1 = resolve_item_in(
            coord.clone(),
            &db,
            &queue,
            test_navidrome(),
            &alice_ctx(),
            &item_symlink,
            action.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(err1.code, FailureCode::InvalidInput);

        let err2 = resolve_item_in(
            coord.clone(),
            &db,
            &queue,
            test_navidrome(),
            &alice_ctx(),
            &item_outside,
            action,
        )
        .await
        .unwrap_err();
        assert_eq!(err2.code, FailureCode::InvalidInput);
        assert_eq!(std::fs::read(&secret).unwrap(), bytes);

        // Dismiss marks item Dismissed and NEVER deletes the file
        let real_file = alice_lib.join("keep_me.flac");
        std::fs::write(&real_file, &bytes).unwrap();
        let (_b3, item_keep) = seed_needs_decision_item(&db, &alice_lib, &real_file).await;
        let dismissed = resolve_item_in(
            coord,
            &db,
            &queue,
            test_navidrome(),
            &alice_ctx(),
            &item_keep,
            LibraryImportResolveAction::Dismiss,
        )
        .await
        .unwrap();
        assert_eq!(dismissed.items[0].status, LibraryImportItemStatus::Dismissed);
        assert_eq!(dismissed.batch.needs_decision_units, 0);
        assert!(real_file.exists());
    }

    #[tokio::test]
    async fn resolve_external_as_is_with_tags_stages_copy_and_leaves_source_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let alice_lib = root.join("music/alice");
        let external = root.join("music/incoming");
        std::fs::create_dir_all(&alice_lib).unwrap();
        std::fs::create_dir_all(&external).unwrap();

        let original_bytes = minimal_flac();
        let external_file = external.join("untagged.flac");
        std::fs::write(&external_file, &original_bytes).unwrap();

        let db = test_db::migrated().await;
        sqlx::query("INSERT INTO users (id, username, password_hash, role) VALUES ('alice', 'alice', 'x', 'user')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO folders (id, user_id, name, path) VALUES ('f-alice', 'alice', 'Music', ?)")
            .bind(alice_lib.to_str().unwrap())
            .execute(&db)
            .await
            .unwrap();

        create_batch_with_items_in(
            &db,
            &NewLibraryImportBatch {
                id: "b-ext".into(),
                user_id: "alice".into(),
                target_folder_id: "f-alice".into(),
                target_path: alice_lib.to_string_lossy().into_owned(),
                source_path: external.to_string_lossy().into_owned(),
                mode: LibraryImportMode::TagAndOrganize,
                status: LibraryImportBatchStatus::Completed,
                total_units: 1,
                total_files: 1,
                already_indexed_files: 0,
                origin: LibraryImportOrigin::Ui,
            },
            &[NewLibraryImportItem {
                id: "i-ext".into(),
                seq: 0,
                source_path: external_file.to_string_lossy().into_owned(),
                display_name: "untagged.flac".into(),
                is_album_dir: false,
                file_count: 1,
            }],
        )
        .await
        .unwrap();
        update_item_result_in(
            &db,
            "b-ext",
            "i-ext",
            LibraryImportItemStatus::NeedsDecision,
            Some("no_match"),
            None,
            None,
        )
        .await
        .unwrap();

        let stub = Arc::new(StubImporter::new());
        stub.script(imported());
        let queue = ImportQueue::new(RecordingDeps::new(stub.clone()));
        let coord = Arc::new(LibraryImportCoordinator::default());

        let detail = resolve_item_in(
            coord,
            &db,
            &queue,
            test_navidrome(),
            &alice_ctx(),
            "i-ext",
            LibraryImportResolveAction::Choose(ImportChoice::AsIsWithTags(AsIsTagOverrides {
                artist: "Four Tet".into(),
                title: "Two Thousand and Seventeen".into(),
                album: Some("New Energy".into()),
            })),
        )
        .await
        .unwrap();

        assert_eq!(detail.items[0].status, LibraryImportItemStatus::Imported);
        // Original external source file is 100% byte-for-byte untouched!
        assert_eq!(std::fs::read(&external_file).unwrap(), original_bytes);
        // Beets was called with CopyOnly from a temporary staged copy, which has now been cleaned up.
        assert_eq!(stub.placements(), vec![ImportPlacement::CopyOnly]);
        let seen = stub.seen();
        assert_eq!(seen.len(), 1);
        let staged_source = &seen[0].0[0];
        assert_ne!(staged_source, &external_file);
        assert_eq!(
            staged_source.file_name().and_then(|n| n.to_str()),
            Some("untagged.flac")
        );
        assert!(!staged_source.exists());
        assert!(!staged_source.parent().unwrap().exists());
    }

    #[tokio::test]
    async fn resolve_cas_blocks_concurrent_calls_and_detached_task_completes_on_caller_drop() {
        use crate::models::library_import::get_item_in;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let alice_lib = root.join("music/alice");
        std::fs::create_dir_all(&alice_lib).unwrap();
        let track = alice_lib.join("song.flac");
        std::fs::write(&track, minimal_flac()).unwrap();

        let db = test_db::migrated().await;
        let (_batch_id, item_id) = seed_needs_decision_item(&db, &alice_lib, &track).await;

        let stub = Arc::new(StubImporter::new().held());
        stub.script(imported());
        let queue = ImportQueue::new(RecordingDeps::new(stub.clone()));
        let coord = Arc::new(LibraryImportCoordinator::default());

        // Start a resolve call and drop its outer future as soon as beets is invoked
        // (simulating an HTTP client disconnect / reverse proxy timeout).
        let db_first = db.clone();
        let queue_first = queue.clone();
        let coord_first = coord.clone();
        let item_first = item_id.clone();
        let caller_handle = tokio::spawn(async move {
            resolve_item_in(
                coord_first,
                &db_first,
                &queue_first,
                test_navidrome(),
                &alice_ctx(),
                &item_first,
                LibraryImportResolveAction::Choose(ImportChoice::AsIs),
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(5), async {
            while stub.active() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("resolve did not enter importer");

        // While item is claimed (`importing`), concurrent Choose and Dismiss both fail with WrongJobState.
        let err_choose = resolve_item_in(
            coord.clone(),
            &db,
            &queue,
            test_navidrome(),
            &alice_ctx(),
            &item_id,
            LibraryImportResolveAction::Choose(ImportChoice::AsIs),
        )
        .await
        .unwrap_err();
        assert_eq!(err_choose.code, FailureCode::WrongJobState);

        let err_dismiss = resolve_item_in(
            coord.clone(),
            &db,
            &queue,
            test_navidrome(),
            &alice_ctx(),
            &item_id,
            LibraryImportResolveAction::Dismiss,
        )
        .await
        .unwrap_err();
        assert_eq!(err_dismiss.code, FailureCode::WrongJobState);

        // Abort the caller future (disconnect) before releasing the importer.
        caller_handle.abort();
        let _ = caller_handle.await;

        // Release the held importer: the detached task MUST finish and persist `imported`.
        stub.release(1);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let it = get_item_in(&db, &item_id).await.unwrap().unwrap();
                if it.status == LibraryImportItemStatus::Imported {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("detached resolve task did not complete after caller disconnect");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_rejects_candidate_and_mbid_on_register_as_is_and_symlinks_in_album_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let alice_lib = root.join("music/alice");
        let outside = root.join("outside");
        let bad_album = alice_lib.join("Artist/BadAlbum");
        for p in [&alice_lib, &outside, &bad_album] {
            std::fs::create_dir_all(p).unwrap();
        }
        let secret = outside.join("secret.flac");
        std::fs::write(&secret, minimal_flac()).unwrap();
        std::fs::write(bad_album.join("01.flac"), minimal_flac()).unwrap();
        std::os::unix::fs::symlink(&secret, bad_album.join("02.flac")).unwrap();
        let reg_file = alice_lib.join("reg.flac");
        std::fs::write(&reg_file, minimal_flac()).unwrap();

        let db = test_db::migrated().await;
        sqlx::query("INSERT INTO users (id, username, password_hash, role) VALUES ('alice', 'alice', 'x', 'user')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO folders (id, user_id, name, path) VALUES ('f-alice', 'alice', 'Music', ?)")
            .bind(alice_lib.to_str().unwrap())
            .execute(&db)
            .await
            .unwrap();

        create_batch_with_items_in(
            &db,
            &NewLibraryImportBatch {
                id: "b-reg".into(),
                user_id: "alice".into(),
                target_folder_id: "f-alice".into(),
                target_path: alice_lib.to_string_lossy().into_owned(),
                source_path: alice_lib.to_string_lossy().into_owned(),
                mode: LibraryImportMode::RegisterAsIs,
                status: LibraryImportBatchStatus::Completed,
                total_units: 1,
                total_files: 1,
                already_indexed_files: 0,
                origin: LibraryImportOrigin::Ui,
            },
            &[NewLibraryImportItem {
                id: "i-reg".into(),
                seq: 0,
                source_path: reg_file.to_string_lossy().into_owned(),
                display_name: "reg.flac".into(),
                is_album_dir: false,
                file_count: 1,
            }],
        )
        .await
        .unwrap();
        let choices = track_choices(vec![candidate("MusicBrainz", "rec-1")]);
        update_item_result_in(
            &db,
            "b-reg",
            "i-reg",
            LibraryImportItemStatus::NeedsDecision,
            Some("no_match"),
            None,
            Some(&choices),
        )
        .await
        .unwrap();

        create_batch_with_items_in(
            &db,
            &NewLibraryImportBatch {
                id: "b-alb".into(),
                user_id: "alice".into(),
                target_folder_id: "f-alice".into(),
                target_path: alice_lib.to_string_lossy().into_owned(),
                source_path: alice_lib.to_string_lossy().into_owned(),
                mode: LibraryImportMode::TagInPlace,
                status: LibraryImportBatchStatus::Completed,
                total_units: 1,
                total_files: 1,
                already_indexed_files: 0,
                origin: LibraryImportOrigin::Ui,
            },
            &[NewLibraryImportItem {
                id: "i-alb".into(),
                seq: 0,
                source_path: bad_album.to_string_lossy().into_owned(),
                display_name: "Artist/BadAlbum".into(),
                is_album_dir: true,
                file_count: 1,
            }],
        )
        .await
        .unwrap();
        update_item_result_in(
            &db,
            "b-alb",
            "i-alb",
            LibraryImportItemStatus::NeedsDecision,
            Some("no_match"),
            None,
            None,
        )
        .await
        .unwrap();

        let stub = Arc::new(StubImporter::new());
        let queue = ImportQueue::new(RecordingDeps::new(stub.clone()));
        let coord = Arc::new(LibraryImportCoordinator::default());

        let err_cand = resolve_item_in(
            coord.clone(),
            &db,
            &queue,
            test_navidrome(),
            &alice_ctx(),
            "i-reg",
            LibraryImportResolveAction::Choose(ImportChoice::Candidate {
                data_source: "MusicBrainz".into(),
                id: "rec-1".into(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err_cand.code, FailureCode::InvalidInput);

        let err_mbid = resolve_item_in(
            coord.clone(),
            &db,
            &queue,
            test_navidrome(),
            &alice_ctx(),
            "i-reg",
            LibraryImportResolveAction::Choose(ImportChoice::MusicBrainz(
                "0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c".into(),
            )),
        )
        .await
        .unwrap_err();
        assert_eq!(err_mbid.code, FailureCode::InvalidInput);

        let err_symlink_alb = resolve_item_in(
            coord,
            &db,
            &queue,
            test_navidrome(),
            &alice_ctx(),
            "i-alb",
            LibraryImportResolveAction::Choose(ImportChoice::AsIs),
        )
        .await
        .unwrap_err();
        assert_eq!(err_symlink_alb.code, FailureCode::InvalidInput);
        assert_eq!(stub.calls(), 0);
    }
}
