-- Batches group the jobs of one request. Web UI requests make one batch per
-- job; the rows of jobs from before this migration get one each.
CREATE TABLE batches (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    label TEXT,
    origin TEXT NOT NULL CHECK (origin IN ('ui', 'mcp'))
);
CREATE INDEX batches_by_user ON batches (user_id, created_at);

ALTER TABLE jobs ADD COLUMN batch_id TEXT REFERENCES batches(id) ON DELETE CASCADE;
ALTER TABLE jobs ADD COLUMN kind TEXT NOT NULL DEFAULT 'track' CHECK (kind IN ('track', 'album'));
ALTER TABLE jobs ADD COLUMN options TEXT;          -- JobOptions JSON, MCP jobs only
ALTER TABLE jobs ADD COLUMN mbid_key TEXT;
ALTER TABLE jobs ADD COLUMN text_key TEXT;
ALTER TABLE jobs ADD COLUMN gate_held INTEGER NOT NULL DEFAULT 0;
ALTER TABLE jobs ADD COLUMN cancel_requested INTEGER NOT NULL DEFAULT 0;
ALTER TABLE jobs ADD COLUMN changed_seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE jobs ADD COLUMN excluded_sources TEXT NOT NULL DEFAULT '[]';
ALTER TABLE jobs ADD COLUMN failure TEXT;          -- Failure JSON of a job that failed before it had a slot
CREATE INDEX jobs_by_batch ON jobs (batch_id);
CREATE INDEX jobs_by_mbid_key ON jobs (user_id, mbid_key);
CREATE INDEX jobs_by_text_key ON jobs (user_id, text_key);

ALTER TABLE job_tracks ADD COLUMN result TEXT;     -- SlotResult JSON
ALTER TABLE job_tracks ADD COLUMN pick_n INTEGER NOT NULL DEFAULT 1;  -- the attempt the slot's current pick starts at
ALTER TABLE job_attempts ADD COLUMN source TEXT;
ALTER TABLE job_attempts ADD COLUMN started_at INTEGER;
ALTER TABLE job_attempts ADD COLUMN ended_at INTEGER;
ALTER TABLE job_attempts ADD COLUMN last_row TEXT;
ALTER TABLE job_attempts ADD COLUMN failure TEXT;

CREATE TABLE job_events (
    job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    at INTEGER NOT NULL,
    stage TEXT NOT NULL,
    event TEXT NOT NULL,                           -- a snake_case name
    facts TEXT NOT NULL,                           -- a JSON object
    PRIMARY KEY (job_id, seq)
);

ALTER TABLE searches ADD COLUMN spec TEXT;         -- StoredSpec JSON, MCP searches only
ALTER TABLE searches ADD COLUMN stats TEXT;        -- SearchStats JSON once ended
ALTER TABLE searches ADD COLUMN failure TEXT;      -- Failure JSON when it failed

CREATE TABLE request_keys (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    grant_id TEXT NOT NULL REFERENCES oauth_grants(id) ON DELETE CASCADE,
    tool TEXT NOT NULL,
    key TEXT NOT NULL,
    args_hash TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    instance TEXT NOT NULL,
    document TEXT,                                 -- NULL while the first call runs
    is_error INTEGER,
    PRIMARY KEY (user_id, grant_id, tool, key)
);

INSERT INTO batches (id, user_id, created_at, label, origin)
    SELECT id, user_id, created_at, batch_label, 'ui' FROM jobs;
UPDATE jobs SET batch_id = id,
    kind = CASE WHEN json_extract(wanted, '$.album') IS NOT NULL THEN 'album' ELSE 'track' END;
