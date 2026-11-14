ALTER TABLE users ADD COLUMN role TEXT NOT NULL DEFAULT 'user' CHECK (role IN ('admin', 'user'));
ALTER TABLE users ADD COLUMN credentials_version INTEGER NOT NULL DEFAULT 0;

UPDATE users SET role = 'admin' WHERE id = '00000000-0000-0000-0000-000000000000';
UPDATE users SET role = 'admin'
WHERE NOT EXISTS (SELECT 1 FROM users WHERE role = 'admin')
  AND rowid = (SELECT MIN(rowid) FROM users);

ALTER TABLE folders ADD COLUMN shared INTEGER NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS folder_roots (
    path TEXT PRIMARY KEY NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- The parent of every admin folder, lexically: drop trailing slashes, the
-- last component, then the slash before it. '/' itself is never approved.
INSERT OR IGNORE INTO folder_roots (path)
SELECT DISTINCT parent FROM (
    SELECT rtrim(rtrim(trimmed, replace(trimmed, '/', '')), '/') AS parent
    FROM (
        SELECT rtrim(f.path, '/') AS trimmed
        FROM folders f JOIN users u ON u.id = f.user_id
        WHERE u.role = 'admin'
    )
)
WHERE parent <> '';
