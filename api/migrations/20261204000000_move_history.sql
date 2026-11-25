CREATE TABLE IF NOT EXISTS move_history (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    song_id TEXT NOT NULL,
    new_song_id TEXT,
    title TEXT NOT NULL,
    artist TEXT NOT NULL,
    album TEXT NOT NULL,
    source_folder_id TEXT NOT NULL,
    source_path TEXT NOT NULL,
    target_folder_id TEXT NOT NULL,
    target_path TEXT NOT NULL,
    origin TEXT NOT NULL,
    client TEXT,
    status TEXT NOT NULL,
    warnings TEXT NOT NULL DEFAULT '[]',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_move_history_user_created
    ON move_history (user_id, created_at DESC);
