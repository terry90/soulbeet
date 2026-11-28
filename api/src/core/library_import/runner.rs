//! Background runner, coordinator, and pause/resume/cancel lifecycle for
//! 1-Click Existing Library Import batches.

use crate::auth::{UserCtx, UserId};
use crate::core::database_failure;
use crate::core::folders;
use crate::core::index::{NavidromeOf, ServerNavidrome};
use crate::core::library_import::scan::{
    resolve_and_validate_source_path_in, scan_unimported_units, validate_unit_before_beets,
};
use crate::core::scan::{start_scan_for, SCAN_ROLES};
use crate::db::DB;
use crate::globals::send_user_event;
use crate::import_queue::{import_failure, Duplicates, ImportJob, ImportQueue, IMPORT_QUEUE};
use crate::models::library_import::{
    cancel_batch_cas_in, create_batch_with_items_in, get_active_batch_for_user_in, get_batch_in,
    get_latest_batch_for_user_in, list_batches_for_user_in, list_items_for_batch_in,
    mark_items_importing_in, next_pending_items_in, recover_interrupted_in,
    reset_importing_items_for_batch_in, reset_items_to_pending_in, transition_batch_status_in,
    update_batch_status_in, update_item_result_in, NewLibraryImportBatch, NewLibraryImportItem,
};
use shared::download::{DownloadEvent, MatchChoices, MatchKind};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::library_import::{
    LibraryImportBatch, LibraryImportBatchDetail, LibraryImportBatchStatus, LibraryImportEvent,
    LibraryImportItem, LibraryImportItemStatus, LibraryImportMode, LibraryImportOrigin,
    LibraryScanPreview,
};
use soulbeet::audio::collect_audio_files;
use soulbeet::{
    DriverChoice, DuplicateRun, FailKind, ImportMode, ImportOutcome, ImportPlacement, ImportResult,
    SkipReason,
};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use tracing::{info, warn};

/// Maximum number of contiguous same-kind units batched into one `register_in_place_batch` call.
const REGISTER_CHUNK_SIZE: usize = 20;

#[derive(Default)]
struct BatchControl {
    paused: AtomicBool,
    cancelled: AtomicBool,
}

#[derive(Default)]
struct ActiveState {
    by_batch: HashMap<String, Arc<BatchControl>>,
    by_user: HashMap<String, String>,
}

enum UnpauseOutcome {
    Unpaused,
    WorkerExited,
    Cancelled,
}

/// Coordinates active library import workers and enforces at most one active batch per user.
#[derive(Default)]
pub struct LibraryImportCoordinator {
    active: Mutex<ActiveState>,
    resolving_items: Mutex<HashSet<String>>,
}

pub static COORDINATOR: LazyLock<Arc<LibraryImportCoordinator>> =
    LazyLock::new(|| Arc::new(LibraryImportCoordinator::default()));

struct ActiveGuard {
    coordinator: Arc<LibraryImportCoordinator>,
    batch_id: String,
    user_id: String,
    ctrl: Arc<BatchControl>,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.coordinator
            .release_ctrl(&self.batch_id, &self.user_id, &self.ctrl);
    }
}

/// RAII guard tracking an item currently being resolved via `resolve_item_in` so a
/// concurrent `resume_batch_in` does not reset it back to `pending`.
pub(crate) struct ResolvingItemGuard {
    coordinator: Arc<LibraryImportCoordinator>,
    item_id: String,
}

impl Drop for ResolvingItemGuard {
    fn drop(&mut self) {
        let mut guard = self
            .coordinator
            .resolving_items
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.remove(&self.item_id);
    }
}

impl LibraryImportCoordinator {
    fn reserve(&self, user_id: &str, batch_id: &str) -> Result<Arc<BatchControl>, Failure> {
        let mut guard = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if guard.by_user.contains_key(user_id) {
            return Err(already_running());
        }
        let ctrl = Arc::new(BatchControl::default());
        guard.by_user.insert(user_id.to_string(), batch_id.to_string());
        guard.by_batch.insert(batch_id.to_string(), ctrl.clone());
        Ok(ctrl)
    }

    fn release_ctrl(&self, batch_id: &str, user_id: &str, ctrl: &Arc<BatchControl>) {
        let mut guard = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if guard
            .by_batch
            .get(batch_id)
            .is_some_and(|existing| Arc::ptr_eq(existing, ctrl))
        {
            guard.by_batch.remove(batch_id);
            if guard.by_user.get(user_id).is_some_and(|id| id == batch_id) {
                guard.by_user.remove(user_id);
            }
        }
    }

    /// Atomically checks whether the worker should exit for pause. While holding the
    /// coordinator lock, if `paused` is still true and `cancelled` is false, releases the
    /// worker's active slot and returns `true` so a concurrent `resume_batch_in` sees either
    /// the active worker (and unpauses it in place) or the released slot (and spawns a new one).
    fn try_release_if_paused(
        &self,
        batch_id: &str,
        user_id: &str,
        ctrl: &Arc<BatchControl>,
    ) -> bool {
        let mut guard = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if !ctrl.paused.load(Ordering::SeqCst) || ctrl.cancelled.load(Ordering::SeqCst) {
            return false;
        }
        if guard
            .by_batch
            .get(batch_id)
            .is_some_and(|existing| Arc::ptr_eq(existing, ctrl))
        {
            guard.by_batch.remove(batch_id);
            if guard.by_user.get(user_id).is_some_and(|id| id == batch_id) {
                guard.by_user.remove(user_id);
            }
        }
        true
    }

    /// Returns the active `BatchControl` for `batch_id` if a worker is still alive, paused,
    /// and not cancelled.
    fn paused_control_for(&self, batch_id: &str) -> Option<Arc<BatchControl>> {
        let guard = self.active.lock().unwrap_or_else(|e| e.into_inner());
        guard.by_batch.get(batch_id).and_then(|ctrl| {
            if ctrl.paused.load(Ordering::SeqCst) && !ctrl.cancelled.load(Ordering::SeqCst) {
                Some(ctrl.clone())
            } else {
                None
            }
        })
    }

    /// Clears `paused` under the coordinator mutex if `ctrl` is still the active worker for
    /// `batch_id` and was not concurrently cancelled.
    fn unpause_if_active(&self, batch_id: &str, ctrl: &Arc<BatchControl>) -> UnpauseOutcome {
        let guard = self.active.lock().unwrap_or_else(|e| e.into_inner());
        let still_active = guard
            .by_batch
            .get(batch_id)
            .is_some_and(|existing| Arc::ptr_eq(existing, ctrl));
        if !still_active {
            return UnpauseOutcome::WorkerExited;
        }
        if ctrl.cancelled.load(Ordering::SeqCst) {
            return UnpauseOutcome::Cancelled;
        }
        ctrl.paused.store(false, Ordering::SeqCst);
        UnpauseOutcome::Unpaused
    }

    fn control_for(&self, batch_id: &str) -> Option<Arc<BatchControl>> {
        let guard = self.active.lock().unwrap_or_else(|e| e.into_inner());
        guard.by_batch.get(batch_id).cloned()
    }

    fn has_active_for_user(&self, user_id: &str) -> bool {
        let guard = self.active.lock().unwrap_or_else(|e| e.into_inner());
        guard.by_user.contains_key(user_id)
    }

    pub(crate) fn track_resolving(self: &Arc<Self>, item_id: &str) -> ResolvingItemGuard {
        let mut guard = self
            .resolving_items
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.insert(item_id.to_string());
        ResolvingItemGuard {
            coordinator: self.clone(),
            item_id: item_id.to_string(),
        }
    }

    pub(crate) fn resolving_item_ids(&self) -> Vec<String> {
        let guard = self
            .resolving_items
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.iter().cloned().collect()
    }

    /// Resets any batches left `running`/`queued` and recovers items left `importing` by a prior process.
    pub async fn recover_on_startup(&self, db: &SqlitePool) {
        match recover_interrupted_in(db).await {
            Ok((0, 0)) => {}
            Ok((batches, items)) => info!(
                "Library import startup recovery: paused {batches} interrupted batch(es) and recovered {items} importing item(s)"
            ),
            Err(e) => warn!("Could not recover interrupted library import batches: {e}"),
        }
    }
}

/// Startup recovery hook wired from `web/src/main.rs`.
pub async fn recover_on_startup() {
    COORDINATOR.recover_on_startup(&DB).await;
}

fn already_running() -> Failure {
    Failure::new(
        FailureCode::AlreadyRunning,
        Stage::Import,
        RetryAdvice::SameSourceLater,
        "A library import batch is already running for this user",
    )
}

pub(crate) fn batch_not_found() -> Failure {
    Failure::new(
        FailureCode::UnknownHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Library import batch not found",
    )
}

pub(crate) fn wrong_batch_state(message: impl Into<String>) -> Failure {
    Failure::new(
        FailureCode::WrongJobState,
        Stage::Import,
        RetryAdvice::NotRetryable,
        message,
    )
}

/// Read-only preview of unimported files for the caller's library or an external source folder.
pub async fn scan_preview(
    actor: &UserCtx,
    source_path: Option<String>,
) -> Result<LibraryScanPreview, Failure> {
    scan_preview_in(&DB, &IMPORT_QUEUE, actor, source_path.as_deref()).await
}

pub async fn scan_preview_in(
    db: &SqlitePool,
    queue: &ImportQueue,
    actor: &UserCtx,
    source_path: Option<&str>,
) -> Result<LibraryScanPreview, Failure> {
    let target_folder = folders::for_user_in(db, &actor.user_id).await?;
    let (source_canon, target_canon, _in_place, exclude_dirs) =
        resolve_and_validate_source_path_in(db, actor, &target_folder, source_path).await?;

    let indexed = queue.indexed_paths(&target_canon).await.map_err(|e| {
        Failure::new(
            FailureCode::ImportBeetsFailed,
            Stage::Import,
            RetryAdvice::SameSourceLater,
            format!("Could not read indexed paths from beets database: {e}"),
        )
    })?;

    let scanned =
        scan_unimported_units(&source_canon, &target_canon, &indexed, &exclude_dirs).await?;
    Ok(scanned.to_preview())
}

/// Starts a new persistent library import batch for `actor`.
pub async fn start_batch(
    actor: &UserCtx,
    source_path: Option<String>,
    mode: LibraryImportMode,
    origin: LibraryImportOrigin,
) -> Result<LibraryImportBatch, Failure> {
    start_batch_in(
        COORDINATOR.clone(),
        &DB,
        IMPORT_QUEUE.clone(),
        Arc::new(ServerNavidrome),
        actor,
        source_path.as_deref(),
        mode,
        origin,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn start_batch_in(
    coordinator: Arc<LibraryImportCoordinator>,
    db: &SqlitePool,
    queue: ImportQueue,
    navidrome: Arc<dyn NavidromeOf>,
    actor: &UserCtx,
    source_path: Option<&str>,
    mode: LibraryImportMode,
    origin: LibraryImportOrigin,
) -> Result<LibraryImportBatch, Failure> {
    let target_folder = folders::for_user_in(db, &actor.user_id).await?;
    let (source_canon, target_canon, in_place, exclude_dirs) =
        resolve_and_validate_source_path_in(db, actor, &target_folder, source_path).await?;

    if !in_place && mode != LibraryImportMode::TagAndOrganize {
        return Err(Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NotRetryable,
            "Importing from an external folder requires tag_and_organize mode so files are copied into your library folder",
        ));
    }

    if coordinator.has_active_for_user(&actor.user_id)
        || get_active_batch_for_user_in(db, &actor.user_id)
            .await
            .map_err(|e| database_failure(Stage::Import, e))?
            .is_some()
    {
        return Err(already_running());
    }

    let indexed = queue.indexed_paths(&target_canon).await.map_err(|e| {
        Failure::new(
            FailureCode::ImportBeetsFailed,
            Stage::Import,
            RetryAdvice::SameSourceLater,
            format!("Could not read indexed paths from beets database: {e}"),
        )
    })?;

    let scanned =
        scan_unimported_units(&source_canon, &target_canon, &indexed, &exclude_dirs).await?;

    let batch_id = uuid::Uuid::new_v4().to_string();
    let new_items: Vec<NewLibraryImportItem> = scanned
        .units
        .iter()
        .enumerate()
        .map(|(idx, u)| NewLibraryImportItem {
            id: uuid::Uuid::new_v4().to_string(),
            seq: idx as u64,
            source_path: u.source_path.to_string_lossy().into_owned(),
            display_name: u.display_name.clone(),
            is_album_dir: u.is_album_dir,
            file_count: u.file_count,
        })
        .collect();

    if new_items.is_empty() {
        let new_batch = NewLibraryImportBatch {
            id: batch_id,
            user_id: actor.user_id.clone(),
            target_folder_id: target_folder.id,
            target_path: target_canon.to_string_lossy().into_owned(),
            source_path: source_canon.to_string_lossy().into_owned(),
            mode,
            status: LibraryImportBatchStatus::Completed,
            total_units: 0,
            total_files: 0,
            already_indexed_files: scanned.already_indexed_files,
            origin,
        };
        let batch = create_batch_with_items_in(db, &new_batch, &[])
            .await
            .map_err(|e| database_failure(Stage::Import, e))?;
        emit_batch_event(&batch, None).await;
        return Ok(batch);
    }

    let control = coordinator.reserve(&actor.user_id, &batch_id)?;
    let new_batch = NewLibraryImportBatch {
        id: batch_id.clone(),
        user_id: actor.user_id.clone(),
        target_folder_id: target_folder.id,
        target_path: target_canon.to_string_lossy().into_owned(),
        source_path: source_canon.to_string_lossy().into_owned(),
        mode,
        status: LibraryImportBatchStatus::Running,
        total_units: new_items.len() as u64,
        total_files: scanned.total_files,
        already_indexed_files: scanned.already_indexed_files,
        origin,
    };

    let batch = match create_batch_with_items_in(db, &new_batch, &new_items).await {
        Ok(b) => b,
        Err(e) => {
            coordinator.release_ctrl(&batch_id, &actor.user_id, &control);
            return Err(database_failure(Stage::Import, e));
        }
    };

    emit_batch_event(&batch, None).await;
    spawn_worker(
        coordinator,
        control,
        db.clone(),
        queue,
        navidrome,
        batch.clone(),
    );
    Ok(batch)
}

fn spawn_worker(
    coordinator: Arc<LibraryImportCoordinator>,
    control: Arc<BatchControl>,
    db: SqlitePool,
    queue: ImportQueue,
    navidrome: Arc<dyn NavidromeOf>,
    batch: LibraryImportBatch,
) {
    tokio::spawn(async move {
        let _guard = ActiveGuard {
            coordinator: coordinator.clone(),
            batch_id: batch.id.clone(),
            user_id: batch.user_id.clone(),
            ctrl: control.clone(),
        };
        run_batch_worker(coordinator, db, queue, navidrome, batch, control).await;
    });
}

pub(crate) fn is_source_in_place(source_path: &str, target_path: &str) -> bool {
    let src = std::fs::canonicalize(source_path).unwrap_or_else(|_| PathBuf::from(source_path));
    let dst = std::fs::canonicalize(target_path).unwrap_or_else(|_| PathBuf::from(target_path));
    src == dst || src.starts_with(&dst)
}

/// Maps a beets `ImportResult` into a library import item status, optional skip kind,
/// typed failure, and optional match choices.
pub(crate) fn classify_import_result(
    result: &ImportResult,
    is_album_dir: bool,
    previous_choices: Option<&MatchChoices>,
) -> (
    LibraryImportItemStatus,
    Option<String>,
    Option<Failure>,
    Option<MatchChoices>,
) {
    let default_kind = if is_album_dir {
        MatchKind::Album
    } else {
        MatchKind::Track
    };
    match &result.outcome {
        ImportOutcome::Imported { .. } => (LibraryImportItemStatus::Imported, None, None, None),
        ImportOutcome::Skipped(SkipReason::AlreadyInLibrary { .. }) => (
            LibraryImportItemStatus::SkippedDuplicate,
            Some("already_in_library".to_string()),
            None,
            None,
        ),
        ImportOutcome::Skipped(SkipReason::NoMatch { .. }) => {
            let choices = result
                .choices
                .clone()
                .or_else(|| previous_choices.cloned())
                .unwrap_or(MatchChoices {
                    kind: default_kind,
                    candidates: vec![],
                    missing_tags: false,
                });
            (
                LibraryImportItemStatus::NeedsDecision,
                Some("no_match".to_string()),
                import_failure(result),
                Some(choices),
            )
        }
        ImportOutcome::Failed {
            kind: FailKind::AsIsMissingTags,
            ..
        } => {
            let mut choices = result
                .choices
                .clone()
                .or_else(|| previous_choices.cloned())
                .unwrap_or(MatchChoices {
                    kind: default_kind,
                    candidates: vec![],
                    missing_tags: true,
                });
            choices.missing_tags = true;
            (
                LibraryImportItemStatus::NeedsDecision,
                Some("as_is_missing_tags".to_string()),
                import_failure(result),
                Some(choices),
            )
        }
        ImportOutcome::Failed {
            kind: FailKind::ChoiceInvalid,
            ..
        } => {
            let choices = result
                .choices
                .clone()
                .or_else(|| previous_choices.cloned());
            (
                LibraryImportItemStatus::NeedsDecision,
                Some("choice_invalid".to_string()),
                import_failure(result),
                choices,
            )
        }
        ImportOutcome::Skipped(SkipReason::NothingToImport)
        | ImportOutcome::LookupFailed { .. }
        | ImportOutcome::Failed { .. }
        | ImportOutcome::TimedOut { .. }
        | ImportOutcome::Refused { .. } => (
            LibraryImportItemStatus::Failed,
            None,
            import_failure(result),
            None,
        ),
    }
}

async fn run_batch_worker(
    coordinator: Arc<LibraryImportCoordinator>,
    db: SqlitePool,
    queue: ImportQueue,
    navidrome: Arc<dyn NavidromeOf>,
    initial_batch: LibraryImportBatch,
    control: Arc<BatchControl>,
) {
    let batch_id = initial_batch.id.clone();
    let target_path = PathBuf::from(&initial_batch.target_path);
    let in_place = is_source_in_place(&initial_batch.source_path, &initial_batch.target_path);
    let mode = initial_batch.mode;

    loop {
        if control.cancelled.load(Ordering::SeqCst) {
            if let Ok(Some(b)) = cancel_batch_cas_in(&db, &batch_id).await {
                emit_batch_event(&b, None).await;
            }
            return;
        }
        if control.paused.load(Ordering::SeqCst)
            && coordinator.try_release_if_paused(&batch_id, &initial_batch.user_id, &control)
        {
            if transition_batch_status_in(
                &db,
                &batch_id,
                LibraryImportBatchStatus::Running,
                LibraryImportBatchStatus::Paused,
                None,
            )
            .await
            .unwrap_or(false)
            {
                if let Ok(Some(b)) = get_batch_in(&db, &batch_id).await {
                    emit_batch_event(&b, None).await;
                }
            }
            return;
        }

        let fetch_limit = if mode == LibraryImportMode::RegisterAsIs {
            REGISTER_CHUNK_SIZE as u32
        } else {
            1
        };
        let pending = match next_pending_items_in(&db, &batch_id, fetch_limit).await {
            Ok(items) => items,
            Err(e) => {
                warn!("Library import batch {batch_id} DB read error: {e}");
                if let Ok(Some(b)) = update_batch_status_in(
                    &db,
                    &batch_id,
                    LibraryImportBatchStatus::Failed,
                    Some(&e),
                )
                .await
                {
                    emit_batch_event(&b, None).await;
                }
                return;
            }
        };

        if pending.is_empty() {
            break;
        }

        match mode {
            LibraryImportMode::TagAndOrganize => {
                let item = &pending[0];
                let _ = mark_items_importing_in(&db, std::slice::from_ref(&item.id)).await;
                let source = match validate_unit_before_beets(&initial_batch, item).await {
                    Ok(canon) => canon,
                    Err(failure) => {
                        if let Ok((b, updated_item)) = update_item_result_in(
                            &db,
                            &batch_id,
                            &item.id,
                            LibraryImportItemStatus::Failed,
                            None,
                            Some(&failure),
                            None,
                        )
                        .await
                        {
                            emit_batch_event(&b, Some(updated_item)).await;
                        }
                        continue;
                    }
                };
                let files = collect_audio_files(&[source.as_path()]).await;
                let import_mode = if item.is_album_dir {
                    ImportMode::Album
                } else {
                    ImportMode::Singleton
                };
                let placement = if in_place {
                    ImportPlacement::MoveInPlace
                } else {
                    ImportPlacement::CopyOnly
                };
                // CRITICAL SAFETY INVARIANT: cleanup_root is ALWAYS None for existing library imports.
                let outcome = queue
                    .submit(ImportJob {
                        sources: vec![source],
                        files,
                        target: target_path.clone(),
                        mode: import_mode,
                        owner: None,
                        snapshot_target: false,
                        cleanup_root: None,
                        choice: DriverChoice::Auto,
                        choices: None,
                        duplicates: Duplicates::Skip,
                        last_run: None,
                        lock_retries: 0,
                        placement,
                    })
                    .await
                    .wait()
                    .await;
                let (status, skip_kind, failure, choices) =
                    classify_import_result(&outcome.result, item.is_album_dir, None);
                if let Ok((b, updated_item)) = update_item_result_in(
                    &db,
                    &batch_id,
                    &item.id,
                    status,
                    skip_kind.as_deref(),
                    failure.as_ref(),
                    choices.as_ref(),
                )
                .await
                {
                    emit_batch_event(&b, Some(updated_item)).await;
                }
            }
            LibraryImportMode::TagInPlace => {
                let item = &pending[0];
                let _ = mark_items_importing_in(&db, std::slice::from_ref(&item.id)).await;
                let source = match validate_unit_before_beets(&initial_batch, item).await {
                    Ok(canon) => canon,
                    Err(failure) => {
                        if let Ok((b, updated_item)) = update_item_result_in(
                            &db,
                            &batch_id,
                            &item.id,
                            LibraryImportItemStatus::Failed,
                            None,
                            Some(&failure),
                            None,
                        )
                        .await
                        {
                            emit_batch_event(&b, Some(updated_item)).await;
                        }
                        continue;
                    }
                };
                let import_mode = if item.is_album_dir {
                    ImportMode::Album
                } else {
                    ImportMode::Singleton
                };
                let result = queue
                    .import_in_place(
                        &target_path,
                        vec![source],
                        import_mode,
                        DriverChoice::Auto,
                        DuplicateRun::Skip,
                    )
                    .await;
                let (status, skip_kind, failure, choices) =
                    classify_import_result(&result, item.is_album_dir, None);
                if let Ok((b, updated_item)) = update_item_result_in(
                    &db,
                    &batch_id,
                    &item.id,
                    status,
                    skip_kind.as_deref(),
                    failure.as_ref(),
                    choices.as_ref(),
                )
                .await
                {
                    emit_batch_event(&b, Some(updated_item)).await;
                }
            }
            LibraryImportMode::RegisterAsIs => {
                let first_is_album = pending[0].is_album_dir;
                let chunk: Vec<LibraryImportItem> = pending
                    .into_iter()
                    .take_while(|it| it.is_album_dir == first_is_album)
                    .collect();
                let ids: Vec<String> = chunk.iter().map(|it| it.id.clone()).collect();
                let _ = mark_items_importing_in(&db, &ids).await;

                // Validate every unit in the chunk before invoking beets.
                let mut valid_chunk: Vec<LibraryImportItem> = Vec::with_capacity(chunk.len());
                let mut sources: Vec<PathBuf> = Vec::with_capacity(chunk.len());
                for item in chunk {
                    match validate_unit_before_beets(&initial_batch, &item).await {
                        Ok(canon) => {
                            sources.push(canon);
                            valid_chunk.push(item);
                        }
                        Err(failure) => {
                            if let Ok((b, updated_item)) = update_item_result_in(
                                &db,
                                &batch_id,
                                &item.id,
                                LibraryImportItemStatus::Failed,
                                None,
                                Some(&failure),
                                None,
                            )
                            .await
                            {
                                emit_batch_event(&b, Some(updated_item)).await;
                            }
                        }
                    }
                }
                if valid_chunk.is_empty() {
                    continue;
                }
                let import_mode = if first_is_album {
                    ImportMode::Album
                } else {
                    ImportMode::Singleton
                };
                let result = queue
                    .register_in_place_batch(&target_path, sources, import_mode)
                    .await;
                process_register_chunk(
                    &db,
                    &queue,
                    &initial_batch,
                    &target_path,
                    &valid_chunk,
                    result,
                    &control,
                )
                .await;
            }
        }
    }

    // Do not overwrite a concurrent cancel or pause that landed during the last item.
    if control.cancelled.load(Ordering::SeqCst) {
        if let Ok(Some(b)) = cancel_batch_cas_in(&db, &batch_id).await {
            emit_batch_event(&b, None).await;
        }
        return;
    }
    if control.paused.load(Ordering::SeqCst)
        && coordinator.try_release_if_paused(&batch_id, &initial_batch.user_id, &control)
    {
        if transition_batch_status_in(
            &db,
            &batch_id,
            LibraryImportBatchStatus::Running,
            LibraryImportBatchStatus::Paused,
            None,
        )
        .await
        .unwrap_or(false)
        {
            if let Ok(Some(b)) = get_batch_in(&db, &batch_id).await {
                emit_batch_event(&b, None).await;
            }
        }
        return;
    }

    if transition_batch_status_in(
        &db,
        &batch_id,
        LibraryImportBatchStatus::Running,
        LibraryImportBatchStatus::Completed,
        None,
    )
    .await
    .unwrap_or(false)
    {
        if let Ok(Some(final_batch)) = get_batch_in(&db, &batch_id).await {
            emit_batch_event(&final_batch, None).await;
            if final_batch.imported_units > 0 {
                trigger_navidrome_scan_best_effort(db, navidrome, UserId::new(&final_batch.user_id));
            }
        }
    }
}

fn canonicalize_or_raw(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Exact matching between a reported beets item path and a library import unit:
/// - For album-directory units, the reported item's immediate parent directory must equal the
///   unit directory (canonicalized on both sides) so nested directories never cross-match.
/// - For singleton units, the reported item path must equal the unit file path.
fn unit_matches_imported_path(unit: &LibraryImportItem, imported_canon: &Path) -> bool {
    let unit_canon = canonicalize_or_raw(Path::new(&unit.source_path));
    if unit.is_album_dir {
        imported_canon.parent() == Some(unit_canon.as_path())
    } else {
        imported_canon == unit_canon
    }
}

async fn process_register_chunk(
    db: &SqlitePool,
    queue: &ImportQueue,
    batch: &LibraryImportBatch,
    target_path: &Path,
    chunk: &[LibraryImportItem],
    result: ImportResult,
    control: &BatchControl,
) {
    let batch_id = &batch.id;

    if chunk.len() == 1 {
        let item = &chunk[0];
        let (status, skip_kind, failure, choices) =
            classify_import_result(&result, item.is_album_dir, None);
        if let Ok((b, updated)) = update_item_result_in(
            db,
            batch_id,
            &item.id,
            status,
            skip_kind.as_deref(),
            failure.as_ref(),
            choices.as_ref(),
        )
        .await
        {
            emit_batch_event(&b, Some(updated)).await;
        }
        return;
    }

    // For multi-unit chunks (len > 1), only mark a unit Imported when beets explicitly
    // reported a matching item path. All unmatched units (and the entire chunk when beets
    // returned AlreadyInLibrary, mixed results, or an error) fall back to 1-by-1 processing
    // so every unit gets its exact per-unit status.
    let mut fallback: Vec<&LibraryImportItem> = Vec::new();
    match &result.outcome {
        ImportOutcome::Imported { items, .. } => {
            let imported_full_paths: Vec<PathBuf> = items
                .iter()
                .map(|it| {
                    let full = if it.path.is_absolute() {
                        it.path.clone()
                    } else {
                        target_path.join(&it.path)
                    };
                    canonicalize_or_raw(&full)
                })
                .collect();

            for item in chunk {
                let matched = imported_full_paths
                    .iter()
                    .any(|p| unit_matches_imported_path(item, p));
                if matched {
                    if let Ok((b, updated)) = update_item_result_in(
                        db,
                        batch_id,
                        &item.id,
                        LibraryImportItemStatus::Imported,
                        None,
                        None,
                        None,
                    )
                    .await
                    {
                        emit_batch_event(&b, Some(updated)).await;
                    }
                } else {
                    fallback.push(item);
                }
            }
        }
        _ => {
            fallback.extend(chunk.iter());
        }
    }

    for (idx, item) in fallback.iter().enumerate() {
        if control.cancelled.load(Ordering::SeqCst) || control.paused.load(Ordering::SeqCst) {
            let remaining_ids: Vec<String> = fallback[idx..]
                .iter()
                .map(|it| it.id.clone())
                .collect();
            let _ = reset_items_to_pending_in(db, &remaining_ids).await;
            return;
        }
        let source = match validate_unit_before_beets(batch, item).await {
            Ok(canon) => canon,
            Err(failure) => {
                if let Ok((b, updated)) = update_item_result_in(
                    db,
                    batch_id,
                    &item.id,
                    LibraryImportItemStatus::Failed,
                    None,
                    Some(&failure),
                    None,
                )
                .await
                {
                    emit_batch_event(&b, Some(updated)).await;
                }
                continue;
            }
        };
        let import_mode = if item.is_album_dir {
            ImportMode::Album
        } else {
            ImportMode::Singleton
        };
        let single_res = queue
            .register_in_place_batch(target_path, vec![source], import_mode)
            .await;
        let (status, skip_kind, failure, choices) =
            classify_import_result(&single_res, item.is_album_dir, None);
        if let Ok((b, updated)) = update_item_result_in(
            db,
            batch_id,
            &item.id,
            status,
            skip_kind.as_deref(),
            failure.as_ref(),
            choices.as_ref(),
        )
        .await
        {
            emit_batch_event(&b, Some(updated)).await;
        }
    }
}

pub(crate) async fn emit_batch_event(
    batch: &LibraryImportBatch,
    latest_item: Option<LibraryImportItem>,
) {
    let user = UserId::new(&batch.user_id);
    let event = DownloadEvent::LibraryImport(Box::new(LibraryImportEvent {
        batch: batch.clone(),
        latest_item,
    }));
    send_user_event(&user, event).await;
}

pub(crate) fn trigger_navidrome_scan_best_effort(
    db: SqlitePool,
    navidrome: Arc<dyn NavidromeOf>,
    user: UserId,
) {
    tokio::spawn(async move {
        let Ok(own) = navidrome.navidrome_of(&user).await else {
            return;
        };
        if let Err(e) = start_scan_for(&db, navidrome.as_ref(), &SCAN_ROLES, &user, &own).await {
            warn!("Library import post-scan failed for {user}: {e}");
        }
    });
}

pub async fn list_batches(actor: &UserCtx) -> Result<Vec<LibraryImportBatch>, Failure> {
    list_batches_for_user_in(&DB, &actor.user_id, 50)
        .await
        .map_err(|e| database_failure(Stage::Import, e))
}

pub async fn get_batch_detail(
    actor: &UserCtx,
    batch_id: &str,
    status: Option<LibraryImportItemStatus>,
    limit: u32,
    offset: u32,
) -> Result<LibraryImportBatchDetail, Failure> {
    get_batch_detail_in(&DB, actor, Some(batch_id), status, limit, offset).await
}

pub async fn get_batch_detail_in(
    db: &SqlitePool,
    actor: &UserCtx,
    batch_id: Option<&str>,
    status: Option<LibraryImportItemStatus>,
    limit: u32,
    offset: u32,
) -> Result<LibraryImportBatchDetail, Failure> {
    let batch = match batch_id {
        Some(id) => get_batch_in(db, id)
            .await
            .map_err(|e| database_failure(Stage::Import, e))?
            .filter(|b| actor.role.is_admin() || b.user_id == actor.user_id)
            .ok_or_else(batch_not_found)?,
        None => get_latest_batch_for_user_in(db, &actor.user_id)
            .await
            .map_err(|e| database_failure(Stage::Import, e))?
            .ok_or_else(batch_not_found)?,
    };

    let (items, total_matching_items) =
        list_items_for_batch_in(db, &batch.id, status, limit.clamp(1, 200), offset)
            .await
            .map_err(|e| database_failure(Stage::Import, e))?;

    Ok(LibraryImportBatchDetail {
        batch,
        items,
        total_matching_items,
    })
}

async fn load_owned_batch(
    db: &SqlitePool,
    actor: &UserCtx,
    batch_id: &str,
) -> Result<LibraryImportBatch, Failure> {
    get_batch_in(db, batch_id)
        .await
        .map_err(|e| database_failure(Stage::Import, e))?
        .filter(|b| actor.role.is_admin() || b.user_id == actor.user_id)
        .ok_or_else(batch_not_found)
}

pub async fn pause_batch(actor: &UserCtx, batch_id: &str) -> Result<LibraryImportBatch, Failure> {
    pause_batch_in(COORDINATOR.clone(), &DB, actor, batch_id).await
}

pub(crate) async fn pause_batch_in(
    coordinator: Arc<LibraryImportCoordinator>,
    db: &SqlitePool,
    actor: &UserCtx,
    batch_id: &str,
) -> Result<LibraryImportBatch, Failure> {
    let batch = load_owned_batch(db, actor, batch_id).await?;
    match batch.status {
        LibraryImportBatchStatus::Paused => Ok(batch),
        LibraryImportBatchStatus::Running | LibraryImportBatchStatus::Queued => {
            let transitioned = transition_batch_status_in(
                db,
                &batch.id,
                batch.status,
                LibraryImportBatchStatus::Paused,
                None,
            )
            .await
            .map_err(|e| database_failure(Stage::Import, e))?;
            if !transitioned {
                let current = load_owned_batch(db, actor, batch_id).await?;
                if current.status == LibraryImportBatchStatus::Paused {
                    return Ok(current);
                }
                return Err(wrong_batch_state(format!(
                    "Cannot pause library import batch in status {}",
                    current.status.as_str()
                )));
            }
            if let Some(ctrl) = coordinator.control_for(&batch.id) {
                ctrl.paused.store(true, Ordering::SeqCst);
            }
            let updated = get_batch_in(db, &batch.id)
                .await
                .map_err(|e| database_failure(Stage::Import, e))?
                .ok_or_else(batch_not_found)?;
            emit_batch_event(&updated, None).await;
            Ok(updated)
        }
        other => Err(wrong_batch_state(format!(
            "Cannot pause library import batch in status {}",
            other.as_str()
        ))),
    }
}

pub async fn resume_batch(actor: &UserCtx, batch_id: &str) -> Result<LibraryImportBatch, Failure> {
    resume_batch_in(
        COORDINATOR.clone(),
        &DB,
        IMPORT_QUEUE.clone(),
        Arc::new(ServerNavidrome),
        actor,
        batch_id,
    )
    .await
}

pub(crate) async fn resume_batch_in(
    coordinator: Arc<LibraryImportCoordinator>,
    db: &SqlitePool,
    queue: ImportQueue,
    navidrome: Arc<dyn NavidromeOf>,
    actor: &UserCtx,
    batch_id: &str,
) -> Result<LibraryImportBatch, Failure> {
    let batch = load_owned_batch(db, actor, batch_id).await?;
    match batch.status {
        LibraryImportBatchStatus::Running => Ok(batch),
        LibraryImportBatchStatus::Paused => {
            // Fast path: if the worker for this exact batch is still finishing its last item
            // before exiting on pause, atomically transition Paused -> Running in DB and clear
            // its pause flag under the coordinator lock without spawning a second worker.
            if let Some(existing_ctrl) = coordinator.paused_control_for(&batch.id) {
                let transitioned = transition_batch_status_in(
                    db,
                    &batch.id,
                    LibraryImportBatchStatus::Paused,
                    LibraryImportBatchStatus::Running,
                    None,
                )
                .await
                .map_err(|e| database_failure(Stage::Import, e))?;
                if !transitioned {
                    let current = load_owned_batch(db, actor, batch_id).await?;
                    if current.status == LibraryImportBatchStatus::Running {
                        return Ok(current);
                    }
                    return Err(wrong_batch_state(format!(
                        "Cannot resume library import batch in status {}",
                        current.status.as_str()
                    )));
                }

                match coordinator.unpause_if_active(&batch.id, &existing_ctrl) {
                    UnpauseOutcome::Unpaused => {
                        let updated = get_batch_in(db, &batch.id)
                            .await
                            .map_err(|e| database_failure(Stage::Import, e))?
                            .ok_or_else(batch_not_found)?;
                        emit_batch_event(&updated, None).await;
                        return Ok(updated);
                    }
                    UnpauseOutcome::Cancelled => {
                        let _ = transition_batch_status_in(
                            db,
                            &batch.id,
                            LibraryImportBatchStatus::Running,
                            LibraryImportBatchStatus::Cancelled,
                            None,
                        )
                        .await;
                        return Err(wrong_batch_state(
                            "Cannot resume library import batch in status cancelled",
                        ));
                    }
                    UnpauseOutcome::WorkerExited => {
                        // The previous worker exited right while we ran the DB CAS.
                        // Reserve a fresh slot and spawn a replacement worker.
                        let control = match coordinator.reserve(&batch.user_id, &batch.id) {
                            Ok(ctrl) => ctrl,
                            Err(e) => {
                                let _ = transition_batch_status_in(
                                    db,
                                    &batch.id,
                                    LibraryImportBatchStatus::Running,
                                    LibraryImportBatchStatus::Paused,
                                    None,
                                )
                                .await;
                                return Err(e);
                            }
                        };
                        let resolving = coordinator.resolving_item_ids();
                        if let Err(e) =
                            reset_importing_items_for_batch_in(db, &batch.id, &resolving).await
                        {
                            coordinator.release_ctrl(&batch.id, &batch.user_id, &control);
                            let _ = transition_batch_status_in(
                                db,
                                &batch.id,
                                LibraryImportBatchStatus::Running,
                                LibraryImportBatchStatus::Paused,
                                None,
                            )
                            .await;
                            return Err(database_failure(Stage::Import, e));
                        }
                        let updated = get_batch_in(db, &batch.id)
                            .await
                            .map_err(|e| database_failure(Stage::Import, e))?
                            .ok_or_else(batch_not_found)?;
                        emit_batch_event(&updated, None).await;
                        spawn_worker(
                            coordinator,
                            control,
                            db.clone(),
                            queue,
                            navidrome,
                            updated.clone(),
                        );
                        return Ok(updated);
                    }
                }
            }

            if coordinator.has_active_for_user(&batch.user_id)
                || get_active_batch_for_user_in(db, &batch.user_id)
                    .await
                    .map_err(|e| database_failure(Stage::Import, e))?
                    .is_some()
            {
                return Err(already_running());
            }
            let control = coordinator.reserve(&batch.user_id, &batch.id)?;
            let transitioned = match transition_batch_status_in(
                db,
                &batch.id,
                LibraryImportBatchStatus::Paused,
                LibraryImportBatchStatus::Running,
                None,
            )
            .await
            {
                Ok(changed) => changed,
                Err(e) => {
                    coordinator.release_ctrl(&batch.id, &batch.user_id, &control);
                    return Err(database_failure(Stage::Import, e));
                }
            };
            if !transitioned {
                coordinator.release_ctrl(&batch.id, &batch.user_id, &control);
                let current = load_owned_batch(db, actor, batch_id).await?;
                if current.status == LibraryImportBatchStatus::Running {
                    return Ok(current);
                }
                return Err(wrong_batch_state(format!(
                    "Cannot resume library import batch in status {}",
                    current.status.as_str()
                )));
            }

            let resolving = coordinator.resolving_item_ids();
            if let Err(e) = reset_importing_items_for_batch_in(db, &batch.id, &resolving).await {
                coordinator.release_ctrl(&batch.id, &batch.user_id, &control);
                let _ = transition_batch_status_in(
                    db,
                    &batch.id,
                    LibraryImportBatchStatus::Running,
                    LibraryImportBatchStatus::Paused,
                    None,
                )
                .await;
                return Err(database_failure(Stage::Import, e));
            }

            let updated = match get_batch_in(db, &batch.id).await {
                Ok(Some(b)) => b,
                Ok(None) => {
                    coordinator.release_ctrl(&batch.id, &batch.user_id, &control);
                    return Err(batch_not_found());
                }
                Err(e) => {
                    coordinator.release_ctrl(&batch.id, &batch.user_id, &control);
                    return Err(database_failure(Stage::Import, e));
                }
            };
            emit_batch_event(&updated, None).await;
            spawn_worker(
                coordinator,
                control,
                db.clone(),
                queue,
                navidrome,
                updated.clone(),
            );
            Ok(updated)
        }
        other => Err(wrong_batch_state(format!(
            "Cannot resume library import batch in status {}",
            other.as_str()
        ))),
    }
}

pub async fn cancel_batch(actor: &UserCtx, batch_id: &str) -> Result<LibraryImportBatch, Failure> {
    cancel_batch_in(COORDINATOR.clone(), &DB, actor, batch_id).await
}

pub(crate) async fn cancel_batch_in(
    coordinator: Arc<LibraryImportCoordinator>,
    db: &SqlitePool,
    actor: &UserCtx,
    batch_id: &str,
) -> Result<LibraryImportBatch, Failure> {
    let batch = load_owned_batch(db, actor, batch_id).await?;
    match batch.status {
        LibraryImportBatchStatus::Cancelled => Ok(batch),
        LibraryImportBatchStatus::Running
        | LibraryImportBatchStatus::Queued
        | LibraryImportBatchStatus::Paused => {
            if let Some(ctrl) = coordinator.control_for(&batch.id) {
                ctrl.cancelled.store(true, Ordering::SeqCst);
            }
            match cancel_batch_cas_in(db, &batch.id)
                .await
                .map_err(|e| database_failure(Stage::Import, e))?
            {
                Some(updated) => {
                    emit_batch_event(&updated, None).await;
                    Ok(updated)
                }
                None => {
                    let current = load_owned_batch(db, actor, batch_id).await?;
                    if current.status == LibraryImportBatchStatus::Cancelled {
                        return Ok(current);
                    }
                    Err(wrong_batch_state(format!(
                        "Cannot cancel library import batch in status {}",
                        current.status.as_str()
                    )))
                }
            }
        }
        other => Err(wrong_batch_state(format!(
            "Cannot cancel library import batch in status {}",
            other.as_str()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Role;
    use crate::db::test_db;
    use crate::import_queue::test_support::{imported, RecordingDeps, StubImporter};
    use crate::test_navidrome::NavidromeFor;
    use std::time::Duration;

    fn alice_ctx() -> UserCtx {
        UserCtx {
            user_id: "alice".into(),
            username: "alice".into(),
            role: Role::User,
        }
    }

    async fn setup_db_and_folder(root: &Path) -> (SqlitePool, PathBuf, PathBuf) {
        let music_root = root.join("music");
        let alice_lib = music_root.join("alice");
        let external = music_root.join("incoming");
        std::fs::create_dir_all(&alice_lib).unwrap();
        std::fs::create_dir_all(&external).unwrap();

        let db = test_db::migrated().await;
        sqlx::query("INSERT INTO users (id, username, password_hash, role) VALUES ('alice', 'alice', 'x', 'user')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO folder_roots (path) VALUES (?)")
            .bind(music_root.to_str().unwrap())
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO folders (id, user_id, name, path) VALUES ('f-alice', 'alice', 'Music', ?)")
            .bind(alice_lib.to_str().unwrap())
            .execute(&db)
            .await
            .unwrap();
        (db, alice_lib, external)
    }

    fn test_navidrome() -> Arc<dyn NavidromeOf> {
        Arc::new(NavidromeFor(Err(|| {
            crate::services::NavidromeLookupError::NotLinked
        })))
    }

    #[tokio::test]
    async fn never_deletes_source_files_on_duplicate_skip_failure_or_cancel_and_uses_correct_placement() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let (db, _alice_lib, external) = setup_db_and_folder(&root).await;

        let f1 = external.join("a_dup.flac");
        let f2 = external.join("b_fail.flac");
        std::fs::write(&f1, b"dup-bytes").unwrap();
        std::fs::write(&f2, b"fail-bytes").unwrap();

        let stub = Arc::new(StubImporter::new());
        stub.script(ImportResult {
            outcome: ImportOutcome::Skipped(SkipReason::AlreadyInLibrary { existing: vec![] }),
            output_tail: String::new(),
            tasks: soulbeet::TaskCount {
                imported: 0,
                total: 1,
            },
            choices: None,
            user_action: None,
            removals: vec![],
            files: vec![],
        });
        stub.script(ImportResult::failed(FailKind::Beets, "corrupt file".into()));
        let queue = ImportQueue::new(RecordingDeps::new(stub.clone()));
        let coord = Arc::new(LibraryImportCoordinator::default());

        let batch = start_batch_in(
            coord.clone(),
            &db,
            queue.clone(),
            test_navidrome(),
            &alice_ctx(),
            Some(external.to_str().unwrap()),
            LibraryImportMode::TagAndOrganize,
            LibraryImportOrigin::Ui,
        )
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let b = get_batch_in(&db, &batch.id).await.unwrap().unwrap();
                if b.status == LibraryImportBatchStatus::Completed {
                    assert_eq!(b.skipped_units, 1);
                    assert_eq!(b.failed_units, 1);
                    assert_eq!(b.completed_units, 2);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("batch did not finish");

        // External TagAndOrganize MUST use CopyOnly and NEVER delete source files.
        assert_eq!(
            stub.placements(),
            vec![ImportPlacement::CopyOnly, ImportPlacement::CopyOnly]
        );
        assert!(f1.exists());
        assert!(f2.exists());
    }

    #[tokio::test]
    async fn startup_recovery_pauses_running_batches_and_resets_importing_items_and_resumes_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let (db, alice_lib, _) = setup_db_and_folder(&root).await;

        let track = alice_lib.join("song.flac");
        std::fs::write(&track, b"audio").unwrap();

        // Simulate a batch left mid-flight by a crashed server process
        let batch_id = "b-interrupted";
        let item_id = "i-interrupted";
        create_batch_with_items_in(
            &db,
            &NewLibraryImportBatch {
                id: batch_id.into(),
                user_id: "alice".into(),
                target_folder_id: "f-alice".into(),
                target_path: alice_lib.to_string_lossy().into_owned(),
                source_path: alice_lib.to_string_lossy().into_owned(),
                mode: LibraryImportMode::TagInPlace,
                status: LibraryImportBatchStatus::Running,
                total_units: 1,
                total_files: 1,
                already_indexed_files: 0,
                origin: LibraryImportOrigin::Ui,
            },
            &[NewLibraryImportItem {
                id: item_id.into(),
                seq: 0,
                source_path: track.to_string_lossy().into_owned(),
                display_name: "song.flac".into(),
                is_album_dir: false,
                file_count: 1,
            }],
        )
        .await
        .unwrap();
        mark_items_importing_in(&db, &[item_id.into()])
            .await
            .unwrap();

        let coord = Arc::new(LibraryImportCoordinator::default());
        coord.recover_on_startup(&db).await;

        let recovered_batch = get_batch_in(&db, batch_id).await.unwrap().unwrap();
        assert_eq!(recovered_batch.status, LibraryImportBatchStatus::Paused);
        let (items, _) = list_items_for_batch_in(&db, batch_id, None, 10, 0)
            .await
            .unwrap();
        assert_eq!(items[0].status, LibraryImportItemStatus::Pending);

        // Resuming the recovered batch completes it cleanly using InPlaceNoMove
        let stub = Arc::new(StubImporter::new());
        stub.script(imported());
        let queue = ImportQueue::new(RecordingDeps::new(stub.clone()));

        let resumed = resume_batch_in(
            coord,
            &db,
            queue,
            test_navidrome(),
            &alice_ctx(),
            batch_id,
        )
        .await
        .unwrap();
        assert_eq!(resumed.status, LibraryImportBatchStatus::Running);

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let b = get_batch_in(&db, batch_id).await.unwrap().unwrap();
                if b.status == LibraryImportBatchStatus::Completed {
                    assert_eq!(b.imported_units, 1);
                    assert_eq!(b.completed_units, 1);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("resumed batch did not complete");

        assert_eq!(stub.placements(), vec![ImportPlacement::InPlaceNoMove]);
        assert!(track.exists());
    }

    #[test]
    fn unit_matches_imported_path_requires_exact_parent_for_album_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let parent = root.join("Artist");
        let child_album = parent.join("Album1");
        std::fs::create_dir_all(&child_album).unwrap();
        let track = child_album.join("01.flac");
        std::fs::write(&track, b"a").unwrap();

        let make_unit = |p: &Path, is_album_dir: bool| LibraryImportItem {
            id: "i".into(),
            batch_id: "b".into(),
            seq: 0,
            source_path: p.to_string_lossy().into_owned(),
            display_name: "u".into(),
            is_album_dir,
            file_count: 1,
            status: LibraryImportItemStatus::Importing,
            skip_kind: None,
            failure: None,
            choices: None,
            updated_at: String::new(),
        };

        // Nested child track must NOT cross-match parent directory unit.
        assert!(!unit_matches_imported_path(&make_unit(&parent, true), &track));
        assert!(unit_matches_imported_path(&make_unit(&child_album, true), &track));
        assert!(unit_matches_imported_path(&make_unit(&track, false), &track));
        assert!(!unit_matches_imported_path(&make_unit(&child_album, false), &track));
    }

    #[tokio::test]
    async fn register_chunk_falls_back_one_by_one_on_duplicate_or_unmatched_items() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let (db, alice_lib, _) = setup_db_and_folder(&root).await;

        let f1 = alice_lib.join("a_dup.flac");
        let f2 = alice_lib.join("b_ok.flac");
        std::fs::write(&f1, b"a").unwrap();
        std::fs::write(&f2, b"b").unwrap();

        let stub = Arc::new(StubImporter::new());
        // 1) Whole 2-item chunk returns AlreadyInLibrary -> MUST fall back 1-by-1!
        stub.script(ImportResult {
            outcome: ImportOutcome::Skipped(SkipReason::AlreadyInLibrary { existing: vec![] }),
            output_tail: String::new(),
            tasks: soulbeet::TaskCount {
                imported: 0,
                total: 2,
            },
            choices: None,
            user_action: None,
            removals: vec![],
            files: vec![],
        });
        // 2) Single fallback for f1 -> AlreadyInLibrary
        stub.script(ImportResult {
            outcome: ImportOutcome::Skipped(SkipReason::AlreadyInLibrary { existing: vec![] }),
            output_tail: String::new(),
            tasks: soulbeet::TaskCount {
                imported: 0,
                total: 1,
            },
            choices: None,
            user_action: None,
            removals: vec![],
            files: vec![],
        });
        // 3) Single fallback for f2 -> Imported (default StubImporter returns matching path)

        let queue = ImportQueue::new(RecordingDeps::new(stub.clone()));
        let coord = Arc::new(LibraryImportCoordinator::default());
        let batch = start_batch_in(
            coord,
            &db,
            queue,
            test_navidrome(),
            &alice_ctx(),
            None,
            LibraryImportMode::RegisterAsIs,
            LibraryImportOrigin::Ui,
        )
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let b = get_batch_in(&db, &batch.id).await.unwrap().unwrap();
                if b.status == LibraryImportBatchStatus::Completed {
                    assert_eq!(b.skipped_units, 1);
                    assert_eq!(b.imported_units, 1);
                    assert_eq!(b.completed_units, 2);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("register chunk fallback batch did not complete");

        // 1 chunk call + 2 individual fallback calls = 3 register calls.
        assert_eq!(stub.register_batches().len(), 3);
    }

    #[tokio::test]
    async fn register_chunk_fallback_honors_pause_and_resets_remaining_items_to_pending() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let (db, alice_lib, _) = setup_db_and_folder(&root).await;

        let f1 = alice_lib.join("01.flac");
        let f2 = alice_lib.join("02.flac");
        let f3 = alice_lib.join("03.flac");
        for f in [&f1, &f2, &f3] {
            std::fs::write(f, b"a").unwrap();
        }

        let batch = create_batch_with_items_in(
            &db,
            &NewLibraryImportBatch {
                id: "b-pause-chunk".into(),
                user_id: "alice".into(),
                target_folder_id: "f-alice".into(),
                target_path: alice_lib.to_string_lossy().into_owned(),
                source_path: alice_lib.to_string_lossy().into_owned(),
                mode: LibraryImportMode::RegisterAsIs,
                status: LibraryImportBatchStatus::Running,
                total_units: 3,
                total_files: 3,
                already_indexed_files: 0,
                origin: LibraryImportOrigin::Ui,
            },
            &[
                NewLibraryImportItem {
                    id: "i1".into(),
                    seq: 0,
                    source_path: f1.to_string_lossy().into_owned(),
                    display_name: "01.flac".into(),
                    is_album_dir: false,
                    file_count: 1,
                },
                NewLibraryImportItem {
                    id: "i2".into(),
                    seq: 1,
                    source_path: f2.to_string_lossy().into_owned(),
                    display_name: "02.flac".into(),
                    is_album_dir: false,
                    file_count: 1,
                },
                NewLibraryImportItem {
                    id: "i3".into(),
                    seq: 2,
                    source_path: f3.to_string_lossy().into_owned(),
                    display_name: "03.flac".into(),
                    is_album_dir: false,
                    file_count: 1,
                },
            ],
        )
        .await
        .unwrap();
        mark_items_importing_in(&db, &["i1".into(), "i2".into(), "i3".into()])
            .await
            .unwrap();

        let (items, _) = list_items_for_batch_in(&db, &batch.id, None, 10, 0)
            .await
            .unwrap();
        let stub = Arc::new(StubImporter::new());
        let queue = ImportQueue::new(RecordingDeps::new(stub.clone()));
        let control = BatchControl::default();
        control.paused.store(true, Ordering::SeqCst);

        // Chunk result fails -> fallback loop sees paused == true immediately and resets all 3 to pending.
        process_register_chunk(
            &db,
            &queue,
            &batch,
            &alice_lib,
            &items,
            ImportResult::failed(FailKind::Beets, "chunk error".into()),
            &control,
        )
        .await;

        assert_eq!(stub.register_batches().len(), 0);
        let (after, _) = list_items_for_batch_in(&db, &batch.id, None, 10, 0)
            .await
            .unwrap();
        assert!(after.iter().all(|it| it.status == LibraryImportItemStatus::Pending));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runner_refuses_album_dir_containing_symlink_without_invoking_beets() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let (db, alice_lib, _) = setup_db_and_folder(&root).await;

        let outside = root.join("secret.flac");
        std::fs::write(&outside, b"secret").unwrap();
        let album = alice_lib.join("Artist/BadAlbum");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("01.flac"), b"ok").unwrap();
        std::os::unix::fs::symlink(&outside, album.join("02.flac")).unwrap();

        let stub = Arc::new(StubImporter::new());
        let queue = ImportQueue::new(RecordingDeps::new(stub.clone()));
        let coord = Arc::new(LibraryImportCoordinator::default());
        let batch = start_batch_in(
            coord,
            &db,
            queue,
            test_navidrome(),
            &alice_ctx(),
            None,
            LibraryImportMode::TagInPlace,
            LibraryImportOrigin::Ui,
        )
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let b = get_batch_in(&db, &batch.id).await.unwrap().unwrap();
                if b.status == LibraryImportBatchStatus::Completed {
                    assert_eq!(b.failed_units, 1);
                    assert_eq!(b.imported_units, 0);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("symlink refusal batch did not finish");

        // Beets was NEVER invoked for the symlink-tainted album directory.
        assert_eq!(stub.calls(), 0);
        let (items, _) = list_items_for_batch_in(&db, &batch.id, None, 10, 0)
            .await
            .unwrap();
        assert_eq!(items[0].status, LibraryImportItemStatus::Failed);
        assert_eq!(
            items[0].failure.as_ref().map(|f| f.code),
            Some(FailureCode::InvalidInput)
        );
    }

    #[tokio::test]
    async fn quick_pause_resume_succeeds_without_already_running_and_cancel_prevents_resurrection() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let (db, alice_lib, _) = setup_db_and_folder(&root).await;

        std::fs::write(alice_lib.join("01.flac"), b"a").unwrap();
        std::fs::write(alice_lib.join("02.flac"), b"b").unwrap();

        let stub = Arc::new(StubImporter::new().held());
        let queue = ImportQueue::new(RecordingDeps::new(stub.clone()));
        let coord = Arc::new(LibraryImportCoordinator::default());

        let batch = start_batch_in(
            coord.clone(),
            &db,
            queue.clone(),
            test_navidrome(),
            &alice_ctx(),
            None,
            LibraryImportMode::TagInPlace,
            LibraryImportOrigin::Ui,
        )
        .await
        .unwrap();

        // Wait until worker is actively inside beets for item 1.
        tokio::time::timeout(Duration::from_secs(5), async {
            while stub.active() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("worker did not enter importer");

        // Pause while item 1 is still running, then immediately resume BEFORE item 1 finishes.
        let paused = pause_batch_in(coord.clone(), &db, &alice_ctx(), &batch.id)
            .await
            .unwrap();
        assert_eq!(paused.status, LibraryImportBatchStatus::Paused);

        let resumed = resume_batch_in(
            coord.clone(),
            &db,
            queue.clone(),
            test_navidrome(),
            &alice_ctx(),
            &batch.id,
        )
        .await
        .unwrap();
        assert_eq!(resumed.status, LibraryImportBatchStatus::Running);

        // Release both items: the original single worker continues and completes the batch!
        stub.release(2);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let b = get_batch_in(&db, &batch.id).await.unwrap().unwrap();
                if b.status == LibraryImportBatchStatus::Completed {
                    assert_eq!(b.imported_units, 2);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("quick pause-resume batch did not complete");
        assert_eq!(stub.max_active(), 1);

        // Now verify cancel prevents resurrection via conditional CAS on a paused batch.
        create_batch_with_items_in(
            &db,
            &NewLibraryImportBatch {
                id: "b-cancel-cas".into(),
                user_id: "alice".into(),
                target_folder_id: "f-alice".into(),
                target_path: alice_lib.to_string_lossy().into_owned(),
                source_path: alice_lib.to_string_lossy().into_owned(),
                mode: LibraryImportMode::TagInPlace,
                status: LibraryImportBatchStatus::Paused,
                total_units: 1,
                total_files: 1,
                already_indexed_files: 0,
                origin: LibraryImportOrigin::Ui,
            },
            &[NewLibraryImportItem {
                id: "i-cancel-cas".into(),
                seq: 0,
                source_path: alice_lib.join("01.flac").to_string_lossy().into_owned(),
                display_name: "01.flac".into(),
                is_album_dir: false,
                file_count: 1,
            }],
        )
        .await
        .unwrap();

        let cancelled = cancel_batch_in(coord.clone(), &db, &alice_ctx(), "b-cancel-cas")
            .await
            .unwrap();
        assert_eq!(cancelled.status, LibraryImportBatchStatus::Cancelled);

        // Attempting to CAS Paused -> Running after cancel returns false and never resurrects.
        assert!(!transition_batch_status_in(
            &db,
            "b-cancel-cas",
            LibraryImportBatchStatus::Paused,
            LibraryImportBatchStatus::Running,
            None,
        )
        .await
        .unwrap());
        let err = resume_batch_in(
            coord,
            &db,
            queue,
            test_navidrome(),
            &alice_ctx(),
            "b-cancel-cas",
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, FailureCode::WrongJobState);
        assert_eq!(
            get_batch_in(&db, "b-cancel-cas").await.unwrap().unwrap().status,
            LibraryImportBatchStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn recovery_recovers_terminal_importing_items_and_resume_skips_actively_resolving_items() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let (db, alice_lib, _) = setup_db_and_folder(&root).await;

        let track = alice_lib.join("song.flac");
        std::fs::write(&track, b"a").unwrap();

        // Terminal batch with an item stuck in `importing` (e.g. crash during resolve_item_in)
        create_batch_with_items_in(
            &db,
            &NewLibraryImportBatch {
                id: "b-term".into(),
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
                id: "i-term".into(),
                seq: 0,
                source_path: track.to_string_lossy().into_owned(),
                display_name: "song.flac".into(),
                is_album_dir: false,
                file_count: 1,
            }],
        )
        .await
        .unwrap();
        mark_items_importing_in(&db, &["i-term".into()]).await.unwrap();

        let coord = Arc::new(LibraryImportCoordinator::default());
        coord.recover_on_startup(&db).await;

        let term_batch = get_batch_in(&db, "b-term").await.unwrap().unwrap();
        assert_eq!(term_batch.status, LibraryImportBatchStatus::Completed);
        assert_eq!(term_batch.needs_decision_units, 1);
        assert_eq!(term_batch.completed_units, 1);
        let (term_items, _) = list_items_for_batch_in(&db, "b-term", None, 10, 0)
            .await
            .unwrap();
        assert_eq!(term_items[0].status, LibraryImportItemStatus::NeedsDecision);

        // Paused batch with 2 importing items: 1 actively resolving (must NOT be reset to pending).
        create_batch_with_items_in(
            &db,
            &NewLibraryImportBatch {
                id: "b-res".into(),
                user_id: "alice".into(),
                target_folder_id: "f-alice".into(),
                target_path: alice_lib.to_string_lossy().into_owned(),
                source_path: alice_lib.to_string_lossy().into_owned(),
                mode: LibraryImportMode::TagInPlace,
                status: LibraryImportBatchStatus::Paused,
                total_units: 2,
                total_files: 2,
                already_indexed_files: 0,
                origin: LibraryImportOrigin::Ui,
            },
            &[
                NewLibraryImportItem {
                    id: "i-resolving".into(),
                    seq: 0,
                    source_path: track.to_string_lossy().into_owned(),
                    display_name: "song.flac".into(),
                    is_album_dir: false,
                    file_count: 1,
                },
                NewLibraryImportItem {
                    id: "i-stale".into(),
                    seq: 1,
                    source_path: track.to_string_lossy().into_owned(),
                    display_name: "song2.flac".into(),
                    is_album_dir: false,
                    file_count: 1,
                },
            ],
        )
        .await
        .unwrap();
        mark_items_importing_in(&db, &["i-resolving".into(), "i-stale".into()])
            .await
            .unwrap();

        let _guard = coord.track_resolving("i-resolving");
        let excluded = coord.resolving_item_ids();
        let reset = reset_importing_items_for_batch_in(&db, "b-res", &excluded)
            .await
            .unwrap();
        assert_eq!(reset, 1);

        let (items, _) = list_items_for_batch_in(&db, "b-res", None, 10, 0)
            .await
            .unwrap();
        assert_eq!(items[0].status, LibraryImportItemStatus::Importing);
        assert_eq!(items[1].status, LibraryImportItemStatus::Pending);
    }
}
