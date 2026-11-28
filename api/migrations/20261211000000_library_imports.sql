-- Persistent batches and per-unit items for 1-Click Existing Library Import.

CREATE TABLE IF NOT EXISTS library_import_batches (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    target_folder_id TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    target_path TEXT NOT NULL,
    source_path TEXT NOT NULL,
    mode TEXT NOT NULL CHECK (mode IN ('tag_and_organize', 'tag_in_place', 'register_as_is')),
    status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'paused', 'completed', 'cancelled', 'failed')),
    total_units INTEGER NOT NULL DEFAULT 0,
    total_files INTEGER NOT NULL DEFAULT 0,
    completed_units INTEGER NOT NULL DEFAULT 0,
    imported_units INTEGER NOT NULL DEFAULT 0,
    needs_decision_units INTEGER NOT NULL DEFAULT 0,
    skipped_units INTEGER NOT NULL DEFAULT 0,
    failed_units INTEGER NOT NULL DEFAULT 0,
    already_indexed_files INTEGER NOT NULL DEFAULT 0,
    origin TEXT NOT NULL CHECK (origin IN ('ui', 'mcp')),
    error TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    finished_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_lib_import_batches_user
    ON library_import_batches(user_id, created_at DESC);

CREATE TABLE IF NOT EXISTS library_import_items (
    id TEXT PRIMARY KEY NOT NULL,
    batch_id TEXT NOT NULL REFERENCES library_import_batches(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    source_path TEXT NOT NULL,
    display_name TEXT NOT NULL,
    is_album_dir INTEGER NOT NULL DEFAULT 0,
    file_count INTEGER NOT NULL DEFAULT 1,
    status TEXT NOT NULL CHECK (status IN ('pending', 'importing', 'imported', 'needs_decision', 'skipped_duplicate', 'failed', 'dismissed')),
    skip_kind TEXT,
    failure_json TEXT,
    choices_json TEXT,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_lib_import_items_batch_status
    ON library_import_items(batch_id, status, seq);
