-- Every download the web UI starts, so its work and its rows survive a
-- restart. Written only by the ledger writer task.
CREATE TABLE jobs (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    folder_id TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    wanted TEXT NOT NULL,              -- DownloadQuery JSON
    import_mode TEXT NOT NULL CHECK (import_mode IN ('album', 'singleton')),
    state TEXT NOT NULL,
    search_id TEXT,
    monitor_batch_id TEXT,             -- the batch whose monitor watches its files
    batch_label TEXT
);
CREATE INDEX jobs_by_user_and_state ON jobs (user_id, state);
CREATE INDEX jobs_by_search ON jobs (search_id);

CREATE TABLE job_tracks (
    job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    slot INTEGER NOT NULL,
    state TEXT NOT NULL,
    last_row TEXT,                     -- the DownloadProgress the UI last got
    import_job TEXT,                   -- StoredImport while importing or kept
    index_wait TEXT,                   -- IndexWait while indexing
    PRIMARY KEY (job_id, slot)
);

CREATE TABLE job_attempts (
    job_id TEXT NOT NULL,
    slot INTEGER NOT NULL,
    n INTEGER NOT NULL,
    candidate TEXT,                    -- DownloadableItem JSON; NULL for an adopted transfer
    PRIMARY KEY (job_id, slot, n),
    FOREIGN KEY (job_id, slot) REFERENCES job_tracks (job_id, slot) ON DELETE CASCADE
);

CREATE TABLE searches (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    backend TEXT,                      -- NULL for an auto-download's search of every backend
    expires_at INTEGER NOT NULL,
    query TEXT NOT NULL,               -- DownloadQuery JSON
    candidates TEXT                    -- Vec<DownloadableGroup> JSON once the search ended
);
