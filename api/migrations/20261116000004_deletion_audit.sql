-- Every file the delete guard removed: who, what, where, the caller's rating, what started the
-- removal (origin) and why (reason). song_id is no longer unique, and the action column, always
-- 'Deleted', goes. Rows from before came from auto-delete.
CREATE TABLE deletion_reviews_rebuilt (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL,
    song_id TEXT NOT NULL,
    title TEXT NOT NULL, artist TEXT NOT NULL, album TEXT NOT NULL,
    path TEXT,
    rating INTEGER,
    origin TEXT NOT NULL,
    reason TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
INSERT INTO deletion_reviews_rebuilt (id, user_id, song_id, title, artist, album, path, rating, origin, reason, created_at)
    SELECT id, user_id, song_id, title, artist, album, path, rating, 'auto', 'disliked', created_at FROM deletion_reviews;
DROP TABLE deletion_reviews;
ALTER TABLE deletion_reviews_rebuilt RENAME TO deletion_reviews;
CREATE INDEX deletion_reviews_by_user ON deletion_reviews (user_id, created_at);
