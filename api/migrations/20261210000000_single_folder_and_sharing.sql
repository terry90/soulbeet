-- Transition to 1 personal library folder per user + hardlink track sharing.
-- Preserves all existing jobs, discovery tracks, fast acquisitions, and archival
-- upgrade jobs by re-pointing every folder_id FK to each user's surviving primary
-- folder before deleting retired folder rows, and records retired/shared paths
-- in legacy_folders so no music files on disk are lost.

CREATE TABLE legacy_folders (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    path TEXT NOT NULL,
    was_shared INTEGER NOT NULL DEFAULT 0,
    target_folder_id TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    migrated_at TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_legacy_folders_user ON legacy_folders(user_id);

-- Normalize trailing slashes on stored folder paths (keep '/' as '/').
UPDATE folders
SET path = CASE
    WHEN rtrim(path, '/') = '' THEN '/'
    ELSE rtrim(path, '/')
END
WHERE path LIKE '%/' AND path <> '/';

-- Preserve effective discovery_enabled state: previously discovery was active
-- only when discovery_enabled = 1 AND a discovery_folder_id was chosen.
UPDATE user_settings
SET discovery_enabled = 0
WHERE discovery_enabled = 1
  AND (discovery_folder_id IS NULL OR discovery_folder_id = '');

-- Deterministically pick one surviving primary folder per existing user:
-- prefer non-shared, then default_download_folder_id, then discovery_folder_id, then oldest row.
-- Filter out orphaned folders whose user_id no longer exists so they never collide on UNIQUE(path).
CREATE TEMP TABLE _primary_folder AS
SELECT user_id, id AS primary_id, shared AS was_shared
FROM (
    SELECT
        f.user_id,
        f.id,
        f.shared,
        ROW_NUMBER() OVER (
            PARTITION BY f.user_id
            ORDER BY
                f.shared ASC,
                CASE WHEN f.id = us.default_download_folder_id THEN 0 ELSE 1 END ASC,
                CASE WHEN f.id = us.discovery_folder_id THEN 0 ELSE 1 END ASC,
                f.rowid ASC
        ) AS rn
    FROM folders f
    LEFT JOIN user_settings us ON us.user_id = f.user_id
    WHERE EXISTS (SELECT 1 FROM users u WHERE u.id = f.user_id)
)
WHERE rn = 1;

-- Record all retired secondary folders for each user before deleting them.
INSERT INTO legacy_folders (id, user_id, name, path, was_shared, target_folder_id)
SELECT f.id, f.user_id, f.name, f.path, f.shared, p.primary_id
FROM folders f
JOIN _primary_folder p ON p.user_id = f.user_id
WHERE f.id <> p.primary_id;

-- Mark Pending discovery_tracks rows whose folder is being retired as 'Removed'
-- so they do not stay permanently wedged in Pending pointing to an old Discovery/ path.
UPDATE discovery_tracks
SET status = 'Removed'
WHERE status = 'Pending'
  AND folder_id NOT IN (SELECT primary_id FROM _primary_folder);

-- Re-point every FK referencing folders(id) to the surviving primary folder so
-- ON DELETE CASCADE never removes existing records when retired rows are deleted.
UPDATE jobs
SET folder_id = COALESCE(
    (SELECT p.primary_id FROM _primary_folder p WHERE p.user_id = jobs.user_id),
    (SELECT p.primary_id FROM folders f JOIN _primary_folder p ON p.user_id = f.user_id WHERE f.id = jobs.folder_id)
)
WHERE folder_id NOT IN (SELECT primary_id FROM _primary_folder)
  AND (
      EXISTS (SELECT 1 FROM _primary_folder p WHERE p.user_id = jobs.user_id)
      OR EXISTS (SELECT 1 FROM folders f JOIN _primary_folder p ON p.user_id = f.user_id WHERE f.id = jobs.folder_id)
  );

UPDATE discovery_tracks
SET folder_id = (
    SELECT p.primary_id
    FROM folders f
    JOIN _primary_folder p ON p.user_id = f.user_id
    WHERE f.id = discovery_tracks.folder_id
)
WHERE folder_id NOT IN (SELECT primary_id FROM _primary_folder)
  AND EXISTS (
      SELECT 1 FROM folders f JOIN _primary_folder p ON p.user_id = f.user_id
      WHERE f.id = discovery_tracks.folder_id
  );

UPDATE fast_acquisitions
SET folder_id = COALESCE(
    (SELECT p.primary_id FROM _primary_folder p WHERE p.user_id = fast_acquisitions.user_id),
    (SELECT p.primary_id FROM folders f JOIN _primary_folder p ON p.user_id = f.user_id WHERE f.id = fast_acquisitions.folder_id)
)
WHERE folder_id NOT IN (SELECT primary_id FROM _primary_folder)
  AND (
      EXISTS (SELECT 1 FROM _primary_folder p WHERE p.user_id = fast_acquisitions.user_id)
      OR EXISTS (SELECT 1 FROM folders f JOIN _primary_folder p ON p.user_id = f.user_id WHERE f.id = fast_acquisitions.folder_id)
  );

UPDATE archival_upgrade_jobs
SET folder_id = COALESCE(
    (SELECT p.primary_id FROM _primary_folder p WHERE p.user_id = archival_upgrade_jobs.user_id),
    (SELECT p.primary_id FROM folders f JOIN _primary_folder p ON p.user_id = f.user_id WHERE f.id = archival_upgrade_jobs.folder_id)
)
WHERE folder_id NOT IN (SELECT primary_id FROM _primary_folder)
  AND (
      EXISTS (SELECT 1 FROM _primary_folder p WHERE p.user_id = archival_upgrade_jobs.user_id)
      OR EXISTS (SELECT 1 FROM folders f JOIN _primary_folder p ON p.user_id = f.user_id WHERE f.id = archival_upgrade_jobs.folder_id)
  );

-- Safe to delete retired secondary rows now that all FKs point to primary_id.
DELETE FROM folders
WHERE id NOT IN (SELECT primary_id FROM _primary_folder);

-- Identify surviving rows that still need a dedicated personal path:
--   1. Users whose only folder had shared = 1
--   2. Duplicate paths shared across distinct users from pre-hardening installs
--      (keep the admin or earliest row on the original path, reassign the rest).
CREATE TEMP TABLE _path_rank AS
SELECT
    f.id,
    f.user_id,
    f.name,
    f.path,
    f.shared,
    u.username,
    ROW_NUMBER() OVER (
        PARTITION BY f.path
        ORDER BY CASE WHEN u.role = 'admin' THEN 0 ELSE 1 END ASC, f.rowid ASC
    ) AS path_rn
FROM folders f
JOIN users u ON u.id = f.user_id;

CREATE TEMP TABLE _reassign AS
SELECT
    r.id,
    r.user_id,
    r.name,
    r.path AS old_path,
    r.username,
    COALESCE(
        NULLIF((SELECT rtrim(path, '/') FROM folder_roots ORDER BY path ASC LIMIT 1), ''),
        NULLIF(rtrim(rtrim(r.path, replace(r.path, '/', '')), '/'), ''),
        '/music'
    ) || '/' || CASE
        WHEN trim(replace(replace(replace(r.username, '/', '_'), '\', '_'), char(0), '_')) IN ('', '.', '..')
        THEN 'user'
        ELSE replace(trim(replace(replace(replace(r.username, '/', '_'), '\', '_'), char(0), '_')), '..', '_')
    END AS candidate_path
FROM _path_rank r
WHERE r.shared = 1 OR r.path_rn > 1;

-- Compute each reassigned user's final unique personal path BEFORE recording
-- legacy_folders so we never insert a self-pointing legacy_folders row (new_path = old_path).
CREATE TEMP TABLE _reassign_final AS
SELECT
    r.id,
    r.user_id,
    r.name,
    r.old_path,
    r.username,
    CASE
        WHEN EXISTS (
            SELECT 1 FROM folders f2
            WHERE f2.path = r.candidate_path AND f2.id <> r.id
        ) OR (
            SELECT COUNT(*) FROM _reassign r2 WHERE r2.candidate_path = r.candidate_path
        ) > 1
        THEN r.candidate_path || '-' || r.id
        ELSE r.candidate_path
    END AS new_path
FROM _reassign r;

-- Mark Pending discovery_tracks rows whose folder path is actually moving as 'Removed'
-- so they never stay wedged in Pending pointing at the old shared/colliding directory.
UPDATE discovery_tracks
SET status = 'Removed'
WHERE status = 'Pending'
  AND folder_id IN (SELECT id FROM _reassign_final WHERE new_path <> old_path);

-- Record original shared/colliding paths in legacy_folders ONLY when the path actually changes.
INSERT INTO legacy_folders (id, user_id, name, path, was_shared, target_folder_id)
SELECT 'legacy-' || r.id, r.user_id, r.name, r.old_path, 1, r.id
FROM _reassign_final r
WHERE r.new_path <> r.old_path;

-- Reassign each shared-only or colliding user to their guaranteed-unique personal path.
-- Startup folder provisioning will create any missing directories on disk.
UPDATE folders
SET
    name = (SELECT r.username FROM _reassign_final r WHERE r.id = folders.id),
    path = (SELECT r.new_path FROM _reassign_final r WHERE r.id = folders.id)
WHERE id IN (SELECT id FROM _reassign_final);

DROP TABLE _reassign_final;
DROP TABLE _reassign;
DROP TABLE _path_rank;
DROP TABLE _primary_folder;

-- Drop legacy move history table (replaced by hardlink track sharing).
DROP TABLE IF EXISTS move_history;

-- Enforce 1 folder per user and unique paths, and drop folders.shared.
DROP INDEX IF EXISTS idx_folders_user_id;
ALTER TABLE folders DROP COLUMN shared;
CREATE UNIQUE INDEX idx_folders_one_per_user ON folders(user_id);
CREATE UNIQUE INDEX idx_folders_unique_path ON folders(path);

-- Drop multi-folder user_settings columns and add incoming share consent toggle.
ALTER TABLE user_settings DROP COLUMN default_download_folder_id;
ALTER TABLE user_settings DROP COLUMN discovery_folder_id;
ALTER TABLE user_settings ADD COLUMN accept_shares INTEGER NOT NULL DEFAULT 1;

-- Track sharing audit and provenance table.
CREATE TABLE track_shares (
    id TEXT PRIMARY KEY NOT NULL,
    batch_id TEXT NOT NULL,
    sender_user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    recipient_user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    source_song_id TEXT NOT NULL,
    target_song_id TEXT,
    source_path TEXT NOT NULL,
    target_path TEXT NOT NULL,
    artist TEXT NOT NULL,
    album TEXT NOT NULL,
    title TEXT NOT NULL,
    link_mode TEXT NOT NULL CHECK (link_mode IN ('hardlink', 'reflink', 'copy')),
    origin TEXT NOT NULL CHECK (origin IN ('ui', 'mcp', 'migration')),
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_track_shares_sender ON track_shares(sender_user_id, created_at DESC);
CREATE INDEX idx_track_shares_recipient ON track_shares(recipient_user_id, created_at DESC);
CREATE INDEX idx_track_shares_batch ON track_shares(batch_id);
