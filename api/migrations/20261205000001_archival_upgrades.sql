-- Tracks fast-acquired (potentially lossy) items so Soulbeet knows which tracks
-- qualify for background archival FLAC upgrades strictly on Keep/Promote/Star.
CREATE TABLE IF NOT EXISTS fast_acquisitions (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    folder_id TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    navidrome_song_id TEXT,
    discovery_track_id TEXT,
    artist TEXT NOT NULL,
    title TEXT NOT NULL,
    album TEXT,
    recording_mbid TEXT,
    backend_id TEXT NOT NULL,
    codec TEXT NOT NULL,
    is_lossy INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_fast_acquisitions_user_song
    ON fast_acquisitions(user_id, navidrome_song_id);
CREATE INDEX IF NOT EXISTS idx_fast_acquisitions_user_discovery
    ON fast_acquisitions(user_id, discovery_track_id);
CREATE INDEX IF NOT EXISTS idx_fast_acquisitions_user_track
    ON fast_acquisitions(user_id, lower(artist), lower(title));

-- Low-priority background archival FLAC upgrade queue. Populated ONLY when a
-- lossy fast-acquired track is explicitly kept, starred, or promoted.
CREATE TABLE IF NOT EXISTS archival_upgrade_jobs (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    folder_id TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    navidrome_song_id TEXT,
    artist TEXT NOT NULL,
    title TEXT NOT NULL,
    album TEXT,
    recording_mbid TEXT,
    original_codec TEXT NOT NULL,
    original_backend TEXT NOT NULL,
    trigger_reason TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'queued',
    job_id TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_archival_upgrades_status
    ON archival_upgrade_jobs(status, updated_at, created_at);
CREATE INDEX IF NOT EXISTS idx_archival_upgrades_user
    ON archival_upgrade_jobs(user_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_archival_upgrades_user_track
    ON archival_upgrade_jobs(user_id, lower(artist), lower(title));
CREATE INDEX IF NOT EXISTS idx_archival_upgrades_user_song
    ON archival_upgrade_jobs(user_id, navidrome_song_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_archival_upgrades_active_unique
    ON archival_upgrade_jobs(user_id, lower(artist), lower(title))
    WHERE status IN ('queued', 'searching', 'downloading');
