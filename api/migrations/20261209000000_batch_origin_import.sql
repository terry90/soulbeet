-- Allow 'import' as a batches.origin value alongside 'ui' and 'mcp'.
-- Temporarily detach jobs.batch_id (nullable) before rebuilding batches so
-- ON DELETE CASCADE never removes existing jobs or their child rows inside
-- sqlx's single atomic migration transaction.
CREATE TEMP TABLE _saved_job_batches AS
SELECT id, batch_id FROM jobs WHERE batch_id IS NOT NULL;

UPDATE jobs SET batch_id = NULL WHERE batch_id IS NOT NULL;

CREATE TABLE batches_new (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    label TEXT,
    origin TEXT NOT NULL CHECK (origin IN ('ui', 'mcp', 'import')),
    playlist_id TEXT,
    playlist_order TEXT,
    playlist_failure TEXT,
    playlist_hidden_dropped INTEGER NOT NULL DEFAULT 0
);

INSERT INTO batches_new (
    id, user_id, created_at, label, origin,
    playlist_id, playlist_order, playlist_failure, playlist_hidden_dropped
)
SELECT
    id, user_id, created_at, label, origin,
    playlist_id, playlist_order, playlist_failure, playlist_hidden_dropped
FROM batches;

DROP TABLE batches;
ALTER TABLE batches_new RENAME TO batches;
CREATE INDEX batches_by_user ON batches (user_id, created_at);

UPDATE jobs
SET batch_id = (
    SELECT batch_id FROM _saved_job_batches WHERE _saved_job_batches.id = jobs.id
)
WHERE id IN (SELECT id FROM _saved_job_batches);

DROP TABLE _saved_job_batches;
