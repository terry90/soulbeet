//! SQLite persistence for 1-Click Existing Library Import batches and items.

use crate::db::DB;
use shared::download::MatchChoices;
use shared::failure::Failure;
use shared::library_import::{
    LibraryImportBatch, LibraryImportBatchStatus, LibraryImportItem, LibraryImportItemStatus,
    LibraryImportMode, LibraryImportOrigin,
};
use sqlx::{FromRow, SqlitePool};

#[derive(Debug, Clone)]
pub struct NewLibraryImportBatch {
    pub id: String,
    pub user_id: String,
    pub target_folder_id: String,
    pub target_path: String,
    pub source_path: String,
    pub mode: LibraryImportMode,
    pub status: LibraryImportBatchStatus,
    pub total_units: u64,
    pub total_files: u64,
    pub already_indexed_files: u64,
    pub origin: LibraryImportOrigin,
}

#[derive(Debug, Clone)]
pub struct NewLibraryImportItem {
    pub id: String,
    pub seq: u64,
    pub source_path: String,
    pub display_name: String,
    pub is_album_dir: bool,
    pub file_count: u64,
}

#[derive(Debug, FromRow)]
struct BatchRow {
    id: String,
    user_id: String,
    target_folder_id: String,
    target_path: String,
    source_path: String,
    mode: String,
    status: String,
    total_units: i64,
    total_files: i64,
    completed_units: i64,
    imported_units: i64,
    needs_decision_units: i64,
    skipped_units: i64,
    failed_units: i64,
    already_indexed_files: i64,
    origin: String,
    error: Option<String>,
    created_at: String,
    updated_at: String,
    finished_at: Option<String>,
}

impl TryFrom<BatchRow> for LibraryImportBatch {
    type Error = String;

    fn try_from(row: BatchRow) -> Result<Self, Self::Error> {
        let mode = LibraryImportMode::from_str(&row.mode)
            .ok_or_else(|| format!("Invalid library import mode in DB: {}", row.mode))?;
        let status = LibraryImportBatchStatus::from_str(&row.status)
            .ok_or_else(|| format!("Invalid library import batch status in DB: {}", row.status))?;
        let origin = LibraryImportOrigin::from_str(&row.origin)
            .ok_or_else(|| format!("Invalid library import origin in DB: {}", row.origin))?;
        Ok(LibraryImportBatch {
            id: row.id,
            user_id: row.user_id,
            target_folder_id: row.target_folder_id,
            target_path: row.target_path,
            source_path: row.source_path,
            mode,
            status,
            total_units: row.total_units.max(0) as u64,
            total_files: row.total_files.max(0) as u64,
            completed_units: row.completed_units.max(0) as u64,
            imported_units: row.imported_units.max(0) as u64,
            needs_decision_units: row.needs_decision_units.max(0) as u64,
            skipped_units: row.skipped_units.max(0) as u64,
            failed_units: row.failed_units.max(0) as u64,
            already_indexed_files: row.already_indexed_files.max(0) as u64,
            origin,
            error: row.error,
            created_at: row.created_at,
            updated_at: row.updated_at,
            finished_at: row.finished_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct ItemRow {
    id: String,
    batch_id: String,
    seq: i64,
    source_path: String,
    display_name: String,
    is_album_dir: i64,
    file_count: i64,
    status: String,
    skip_kind: Option<String>,
    failure_json: Option<String>,
    choices_json: Option<String>,
    updated_at: String,
}

impl TryFrom<ItemRow> for LibraryImportItem {
    type Error = String;

    fn try_from(row: ItemRow) -> Result<Self, Self::Error> {
        let status = LibraryImportItemStatus::from_str(&row.status)
            .ok_or_else(|| format!("Invalid library import item status in DB: {}", row.status))?;
        let failure = match row.failure_json {
            Some(ref raw) if !raw.is_empty() => serde_json::from_str::<Failure>(raw).ok(),
            _ => None,
        };
        let choices = match row.choices_json {
            Some(ref raw) if !raw.is_empty() => serde_json::from_str::<MatchChoices>(raw).ok(),
            _ => None,
        };
        Ok(LibraryImportItem {
            id: row.id,
            batch_id: row.batch_id,
            seq: row.seq.max(0) as u64,
            source_path: row.source_path,
            display_name: row.display_name,
            is_album_dir: row.is_album_dir != 0,
            file_count: row.file_count.max(0) as u64,
            status,
            skip_kind: row.skip_kind,
            failure,
            choices,
            updated_at: row.updated_at,
        })
    }
}

const BATCH_SELECT: &str = "SELECT id, user_id, target_folder_id, target_path, source_path, \
    mode, status, total_units, total_files, completed_units, imported_units, \
    needs_decision_units, skipped_units, failed_units, already_indexed_files, \
    origin, error, created_at, updated_at, finished_at \
    FROM library_import_batches";

const ITEM_SELECT: &str = "SELECT id, batch_id, seq, source_path, display_name, \
    is_album_dir, file_count, status, skip_kind, failure_json, choices_json, updated_at \
    FROM library_import_items";

/// Creates a library import batch and all its initial items atomically in one transaction.
pub async fn create_batch_with_items_in(
    db: &SqlitePool,
    batch: &NewLibraryImportBatch,
    items: &[NewLibraryImportItem],
) -> Result<LibraryImportBatch, String> {
    let mut tx = db.begin().await.map_err(|e| e.to_string())?;
    let finished_at_sql = if batch.status.is_terminal() {
        "datetime('now')"
    } else {
        "NULL"
    };
    let insert_batch = format!(
        "INSERT INTO library_import_batches ( \
            id, user_id, target_folder_id, target_path, source_path, mode, status, \
            total_units, total_files, already_indexed_files, origin, finished_at \
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, {finished_at_sql})"
    );
    sqlx::query(&insert_batch)
        .bind(&batch.id)
        .bind(&batch.user_id)
        .bind(&batch.target_folder_id)
        .bind(&batch.target_path)
        .bind(&batch.source_path)
        .bind(batch.mode.as_str())
        .bind(batch.status.as_str())
        .bind(batch.total_units as i64)
        .bind(batch.total_files as i64)
        .bind(batch.already_indexed_files as i64)
        .bind(batch.origin.as_str())
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    for item in items {
        sqlx::query(
            "INSERT INTO library_import_items ( \
                id, batch_id, seq, source_path, display_name, is_album_dir, file_count, status \
            ) VALUES (?, ?, ?, ?, ?, ?, ?, 'pending')",
        )
        .bind(&item.id)
        .bind(&batch.id)
        .bind(item.seq as i64)
        .bind(&item.source_path)
        .bind(&item.display_name)
        .bind(if item.is_album_dir { 1i64 } else { 0i64 })
        .bind(item.file_count as i64)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    }

    let query = format!("{BATCH_SELECT} WHERE id = ?");
    let row = sqlx::query_as::<_, BatchRow>(&query)
        .bind(&batch.id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;
    LibraryImportBatch::try_from(row)
}

pub async fn get_batch(id: &str) -> Result<Option<LibraryImportBatch>, String> {
    get_batch_in(&DB, id).await
}

pub async fn get_batch_in(db: &SqlitePool, id: &str) -> Result<Option<LibraryImportBatch>, String> {
    let query = format!("{BATCH_SELECT} WHERE id = ?");
    let row = sqlx::query_as::<_, BatchRow>(&query)
        .bind(id)
        .fetch_optional(db)
        .await
        .map_err(|e| e.to_string())?;
    row.map(LibraryImportBatch::try_from).transpose()
}

pub async fn list_batches_for_user(
    user_id: &str,
    limit: u32,
) -> Result<Vec<LibraryImportBatch>, String> {
    list_batches_for_user_in(&DB, user_id, limit).await
}

pub async fn list_batches_for_user_in(
    db: &SqlitePool,
    user_id: &str,
    limit: u32,
) -> Result<Vec<LibraryImportBatch>, String> {
    let query =
        format!("{BATCH_SELECT} WHERE user_id = ? ORDER BY created_at DESC, rowid DESC LIMIT ?");
    let rows = sqlx::query_as::<_, BatchRow>(&query)
        .bind(user_id)
        .bind(i64::from(limit))
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;
    rows.into_iter().map(LibraryImportBatch::try_from).collect()
}

pub async fn get_active_batch_for_user_in(
    db: &SqlitePool,
    user_id: &str,
) -> Result<Option<LibraryImportBatch>, String> {
    let query = format!(
        "{BATCH_SELECT} WHERE user_id = ? AND status IN ('queued', 'running') \
         ORDER BY created_at DESC, rowid DESC LIMIT 1"
    );
    let row = sqlx::query_as::<_, BatchRow>(&query)
        .bind(user_id)
        .fetch_optional(db)
        .await
        .map_err(|e| e.to_string())?;
    row.map(LibraryImportBatch::try_from).transpose()
}

pub async fn get_latest_batch_for_user_in(
    db: &SqlitePool,
    user_id: &str,
) -> Result<Option<LibraryImportBatch>, String> {
    let query = format!(
        "{BATCH_SELECT} WHERE user_id = ? ORDER BY created_at DESC, rowid DESC LIMIT 1"
    );
    let row = sqlx::query_as::<_, BatchRow>(&query)
        .bind(user_id)
        .fetch_optional(db)
        .await
        .map_err(|e| e.to_string())?;
    row.map(LibraryImportBatch::try_from).transpose()
}

pub async fn update_batch_status_in(
    db: &SqlitePool,
    batch_id: &str,
    status: LibraryImportBatchStatus,
    error: Option<&str>,
) -> Result<Option<LibraryImportBatch>, String> {
    let finished_expr = if status.is_terminal() {
        "COALESCE(finished_at, datetime('now'))"
    } else {
        "NULL"
    };
    let sql = format!(
        "UPDATE library_import_batches \
         SET status = ?, error = ?, updated_at = datetime('now'), finished_at = {finished_expr} \
         WHERE id = ?"
    );
    sqlx::query(&sql)
        .bind(status.as_str())
        .bind(error)
        .bind(batch_id)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;
    get_batch_in(db, batch_id).await
}

/// Atomically transitions a batch's status from `from` to `to` if and only if
/// its current status equals `from`. Returns `true` when the transition occurred.
pub async fn transition_batch_status_in(
    db: &SqlitePool,
    batch_id: &str,
    from: LibraryImportBatchStatus,
    to: LibraryImportBatchStatus,
    error: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let finished_expr = if to.is_terminal() {
        "COALESCE(finished_at, datetime('now'))"
    } else {
        "NULL"
    };
    let sql = format!(
        "UPDATE library_import_batches \
         SET status = ?, error = ?, updated_at = datetime('now'), finished_at = {finished_expr} \
         WHERE id = ? AND status = ?"
    );
    let res = sqlx::query(&sql)
        .bind(to.as_str())
        .bind(error)
        .bind(batch_id)
        .bind(from.as_str())
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Atomically cancels a batch only if it is still in `queued`, `running`, or `paused`.
/// Returns `Ok(Some(batch))` if cancelled, or `Ok(None)` if it was already terminal or missing.
pub async fn cancel_batch_cas_in(
    db: &SqlitePool,
    batch_id: &str,
) -> Result<Option<LibraryImportBatch>, String> {
    let res = sqlx::query(
        "UPDATE library_import_batches \
         SET status = 'cancelled', error = NULL, updated_at = datetime('now'), \
             finished_at = COALESCE(finished_at, datetime('now')) \
         WHERE id = ? AND status IN ('queued', 'running', 'paused')",
    )
    .bind(batch_id)
    .execute(db)
    .await
    .map_err(|e| e.to_string())?;
    if res.rows_affected() == 0 {
        return Ok(None);
    }
    get_batch_in(db, batch_id).await
}

pub async fn list_items_for_batch_in(
    db: &SqlitePool,
    batch_id: &str,
    status: Option<LibraryImportItemStatus>,
    limit: u32,
    offset: u32,
) -> Result<(Vec<LibraryImportItem>, u64), String> {
    let (rows, count): (Vec<ItemRow>, i64) = match status {
        Some(st) => {
            let count_val: (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM library_import_items WHERE batch_id = ? AND status = ?",
            )
            .bind(batch_id)
            .bind(st.as_str())
            .fetch_one(db)
            .await
            .map_err(|e| e.to_string())?;

            let query = format!(
                "{ITEM_SELECT} WHERE batch_id = ? AND status = ? ORDER BY seq ASC LIMIT ? OFFSET ?"
            );
            let rows = sqlx::query_as::<_, ItemRow>(&query)
                .bind(batch_id)
                .bind(st.as_str())
                .bind(i64::from(limit))
                .bind(i64::from(offset))
                .fetch_all(db)
                .await
                .map_err(|e| e.to_string())?;
            (rows, count_val.0)
        }
        None => {
            let count_val: (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM library_import_items WHERE batch_id = ?")
                    .bind(batch_id)
                    .fetch_one(db)
                    .await
                    .map_err(|e| e.to_string())?;

            let query =
                format!("{ITEM_SELECT} WHERE batch_id = ? ORDER BY seq ASC LIMIT ? OFFSET ?");
            let rows = sqlx::query_as::<_, ItemRow>(&query)
                .bind(batch_id)
                .bind(i64::from(limit))
                .bind(i64::from(offset))
                .fetch_all(db)
                .await
                .map_err(|e| e.to_string())?;
            (rows, count_val.0)
        }
    };
    let items = rows
        .into_iter()
        .map(LibraryImportItem::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    Ok((items, count.max(0) as u64))
}

pub async fn get_item_in(db: &SqlitePool, item_id: &str) -> Result<Option<LibraryImportItem>, String> {
    let query = format!("{ITEM_SELECT} WHERE id = ?");
    let row = sqlx::query_as::<_, ItemRow>(&query)
        .bind(item_id)
        .fetch_optional(db)
        .await
        .map_err(|e| e.to_string())?;
    row.map(LibraryImportItem::try_from).transpose()
}

pub async fn get_item_for_user_in(
    db: &SqlitePool,
    item_id: &str,
    user_id: &str,
    is_admin: bool,
) -> Result<Option<(LibraryImportBatch, LibraryImportItem)>, String> {
    let Some(item) = get_item_in(db, item_id).await? else {
        return Ok(None);
    };
    let Some(batch) = get_batch_in(db, &item.batch_id).await? else {
        return Ok(None);
    };
    if !is_admin && batch.user_id != user_id {
        return Ok(None);
    }
    Ok(Some((batch, item)))
}

pub async fn next_pending_items_in(
    db: &SqlitePool,
    batch_id: &str,
    limit: u32,
) -> Result<Vec<LibraryImportItem>, String> {
    let query = format!(
        "{ITEM_SELECT} WHERE batch_id = ? AND status = 'pending' ORDER BY seq ASC LIMIT ?"
    );
    let rows = sqlx::query_as::<_, ItemRow>(&query)
        .bind(batch_id)
        .bind(i64::from(limit))
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;
    rows.into_iter().map(LibraryImportItem::try_from).collect()
}

pub async fn mark_items_importing_in(db: &SqlitePool, item_ids: &[String]) -> Result<(), String> {
    if item_ids.is_empty() {
        return Ok(());
    }
    let mut tx = db.begin().await.map_err(|e| e.to_string())?;
    for id in item_ids {
        sqlx::query(
            "UPDATE library_import_items \
             SET status = 'importing', updated_at = datetime('now') \
             WHERE id = ?",
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    }
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Atomically claims a `needs_decision` or `failed` item for resolution by transitioning
/// it to `importing`. Returns `true` if this caller claimed the item.
pub async fn claim_item_for_resolve_in(
    db: &SqlitePool,
    item_id: &str,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE library_import_items \
         SET status = 'importing', updated_at = datetime('now') \
         WHERE id = ? AND status IN ('needs_decision', 'failed')",
    )
    .bind(item_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Atomically dismisses a `needs_decision` or `failed` item and recalculates its parent
/// batch counters. Returns `Ok(None)` if the item was concurrently claimed or resolved.
pub async fn dismiss_item_cas_in(
    db: &SqlitePool,
    batch_id: &str,
    item_id: &str,
) -> Result<Option<(LibraryImportBatch, LibraryImportItem)>, String> {
    let mut tx = db.begin().await.map_err(|e| e.to_string())?;
    let res = sqlx::query(
        "UPDATE library_import_items \
         SET status = 'dismissed', failure_json = NULL, choices_json = NULL, \
             updated_at = datetime('now') \
         WHERE id = ? AND batch_id = ? AND status IN ('needs_decision', 'failed')",
    )
    .bind(item_id)
    .bind(batch_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    if res.rows_affected() == 0 {
        tx.rollback().await.map_err(|e| e.to_string())?;
        return Ok(None);
    }

    recalc_batch_counters_tx(&mut tx, batch_id).await?;

    let batch_q = format!("{BATCH_SELECT} WHERE id = ?");
    let batch_row = sqlx::query_as::<_, BatchRow>(&batch_q)
        .bind(batch_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    let item_q = format!("{ITEM_SELECT} WHERE id = ?");
    let item_row = sqlx::query_as::<_, ItemRow>(&item_q)
        .bind(item_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(Some((
        LibraryImportBatch::try_from(batch_row)?,
        LibraryImportItem::try_from(item_row)?,
    )))
}

/// Resets specific `importing` items back to `pending` (e.g. when a register chunk stops
/// mid-fallback due to pause or cancel).
pub async fn reset_items_to_pending_in(
    db: &SqlitePool,
    item_ids: &[String],
) -> Result<u64, String> {
    if item_ids.is_empty() {
        return Ok(0);
    }
    let mut tx = db.begin().await.map_err(|e| e.to_string())?;
    let mut updated = 0u64;
    for id in item_ids {
        let res = sqlx::query(
            "UPDATE library_import_items \
             SET status = 'pending', updated_at = datetime('now') \
             WHERE id = ? AND status = 'importing'",
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        updated += res.rows_affected();
    }
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(updated)
}

pub async fn reset_importing_items_for_batch_in(
    db: &SqlitePool,
    batch_id: &str,
    exclude_item_ids: &[String],
) -> Result<u64, String> {
    if exclude_item_ids.is_empty() {
        let res = sqlx::query(
            "UPDATE library_import_items \
             SET status = 'pending', updated_at = datetime('now') \
             WHERE batch_id = ? AND status = 'importing'",
        )
        .bind(batch_id)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;
        return Ok(res.rows_affected());
    }

    let placeholders = vec!["?"; exclude_item_ids.len()].join(", ");
    let sql = format!(
        "UPDATE library_import_items \
         SET status = 'pending', updated_at = datetime('now') \
         WHERE batch_id = ? AND status = 'importing' AND id NOT IN ({placeholders})"
    );
    let mut query = sqlx::query(&sql).bind(batch_id);
    for id in exclude_item_ids {
        query = query.bind(id);
    }
    let res = query.execute(db).await.map_err(|e| e.to_string())?;
    Ok(res.rows_affected())
}

async fn recalc_batch_counters_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    batch_id: &str,
) -> Result<(), String> {
    sqlx::query(
        "UPDATE library_import_batches \
         SET \
             completed_units = (SELECT COUNT(*) FROM library_import_items WHERE batch_id = ? AND status NOT IN ('pending', 'importing')), \
             imported_units = (SELECT COUNT(*) FROM library_import_items WHERE batch_id = ? AND status = 'imported'), \
             needs_decision_units = (SELECT COUNT(*) FROM library_import_items WHERE batch_id = ? AND status = 'needs_decision'), \
             skipped_units = (SELECT COUNT(*) FROM library_import_items WHERE batch_id = ? AND status = 'skipped_duplicate'), \
             failed_units = (SELECT COUNT(*) FROM library_import_items WHERE batch_id = ? AND status = 'failed'), \
             updated_at = datetime('now') \
         WHERE id = ?",
    )
    .bind(batch_id)
    .bind(batch_id)
    .bind(batch_id)
    .bind(batch_id)
    .bind(batch_id)
    .bind(batch_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Atomically updates an item's status/outcome and recalculates the parent batch's
/// aggregate counters from `library_import_items` so counters never drift.
pub async fn update_item_result_in(
    db: &SqlitePool,
    batch_id: &str,
    item_id: &str,
    status: LibraryImportItemStatus,
    skip_kind: Option<&str>,
    failure: Option<&Failure>,
    choices: Option<&MatchChoices>,
) -> Result<(LibraryImportBatch, LibraryImportItem), String> {
    let failure_json = failure
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| e.to_string())?;
    let choices_json = choices
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| e.to_string())?;

    let mut tx = db.begin().await.map_err(|e| e.to_string())?;

    sqlx::query(
        "UPDATE library_import_items \
         SET status = ?, skip_kind = ?, failure_json = ?, choices_json = ?, \
             updated_at = datetime('now') \
         WHERE id = ? AND batch_id = ?",
    )
    .bind(status.as_str())
    .bind(skip_kind)
    .bind(failure_json)
    .bind(choices_json)
    .bind(item_id)
    .bind(batch_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    recalc_batch_counters_tx(&mut tx, batch_id).await?;

    let batch_q = format!("{BATCH_SELECT} WHERE id = ?");
    let batch_row = sqlx::query_as::<_, BatchRow>(&batch_q)
        .bind(batch_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    let item_q = format!("{ITEM_SELECT} WHERE id = ?");
    let item_row = sqlx::query_as::<_, ItemRow>(&item_q)
        .bind(item_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;
    Ok((
        LibraryImportBatch::try_from(batch_row)?,
        LibraryImportItem::try_from(item_row)?,
    ))
}

/// Resets any batches left `running` or `queued` by a previous server process to `paused`,
/// resets `importing` items on active/paused batches to `pending`, and recovers `importing`
/// items on terminal batches (`completed`, `cancelled`, `failed`) to `needs_decision` while
/// recalculating their batch counters so no item is ever orphaned.
pub async fn recover_interrupted_in(db: &SqlitePool) -> Result<(u64, u64), String> {
    let mut tx = db.begin().await.map_err(|e| e.to_string())?;

    let terminal_batch_ids: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT batch_id FROM library_import_items \
         WHERE status = 'importing' \
           AND batch_id IN (SELECT id FROM library_import_batches WHERE status IN ('completed', 'cancelled', 'failed'))",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    let active_items_res = sqlx::query(
        "UPDATE library_import_items \
         SET status = 'pending', updated_at = datetime('now') \
         WHERE status = 'importing' \
           AND batch_id IN (SELECT id FROM library_import_batches WHERE status IN ('running', 'queued', 'paused'))",
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    let terminal_items_res = sqlx::query(
        "UPDATE library_import_items \
         SET status = 'needs_decision', updated_at = datetime('now') \
         WHERE status = 'importing' \
           AND batch_id IN (SELECT id FROM library_import_batches WHERE status IN ('completed', 'cancelled', 'failed'))",
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    for (batch_id,) in &terminal_batch_ids {
        recalc_batch_counters_tx(&mut tx, batch_id).await?;
    }

    let batches_res = sqlx::query(
        "UPDATE library_import_batches \
         SET status = 'paused', updated_at = datetime('now') \
         WHERE status IN ('running', 'queued')",
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;
    Ok((
        batches_res.rows_affected(),
        active_items_res.rows_affected() + terminal_items_res.rows_affected(),
    ))
}
