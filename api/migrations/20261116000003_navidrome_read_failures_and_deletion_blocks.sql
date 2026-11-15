-- The accounts the delete guard could not read another listener's ratings of, one row each,
-- cleared by their next successful read. Admins see them in Settings > Library.
CREATE TABLE navidrome_read_failures (
    user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    error TEXT NOT NULL,
    since TEXT NOT NULL DEFAULT (datetime('now'))
);

-- The tracks auto-delete kept at a user's last sync, and why.
CREATE TABLE deletion_blocks (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    song_id TEXT NOT NULL,
    title TEXT NOT NULL,
    artist TEXT NOT NULL,
    message TEXT NOT NULL,
    PRIMARY KEY (user_id, song_id)
);
